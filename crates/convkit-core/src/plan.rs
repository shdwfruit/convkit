use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::media;
use crate::probe::MediaProbe;
use crate::recipe::ScaleStyle;
use crate::resolve::AvailableBackends;
use crate::video::{Enlargement, ResolvedVideo, Target};
use crate::{registry, Arg, Backend, ConvError, ErrorCode, Format, OutputMode, Recipe, Tuning};

/// The first argv element `build` inserts for every `Soffice` step, in
/// place of the real `-env:UserInstallation=<url>` `exec::run` actually
/// passes. `exec::run` injected this flag after rendering the plan, so it
/// never appeared in `--dry-run`'s printed argv (I1) — and the README
/// published such a command: a user copying
/// `soffice --headless --norestore --convert-to pdf --outdir . r.docx` runs
/// LibreOffice against their live profile, the exact collision this flag
/// exists to prevent, which fails outright if LibreOffice is already open.
///
/// `build` can't know the real per-run profile path — that only exists once
/// `exec::run` computes an isolated profile directory for this specific
/// soffice invocation — so it emits this placeholder instead, keeping the
/// printed command honest about the flag's *presence* while `exec::run`
/// substitutes the real, isolated URL in for it at execution time (see
/// `exec::run`'s `debug_assert_eq!` against this constant). `plan::build`
/// stays pure either way: no filesystem access, no real profile path, just
/// this fixed string.
pub const USER_INSTALLATION_PLACEHOLDER: &str = "-env:UserInstallation=<per-run temp profile>";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStep {
    pub backend: Backend,
    /// Bare executable name, never a resolved path. Keeps plans machine-independent.
    pub program: String,
    pub argv: Vec<String>,
    pub output_mode: OutputMode,
    /// The path this step actually writes: the final output for the last
    /// step, the intermediate path for every earlier step. The execution
    /// engine relies on this rather than reading the last argv
    /// element, which is wrong for `soffice` recipes — their argv ends with
    /// the *input* path, not the output.
    pub output: PathBuf,
    pub intermediate_ext: Option<String>,
    /// Indices into `argv` holding filesystem paths, for the executor's
    /// Windows extended-path rewriting (F193). `serde(skip)`: this is
    /// execution detail, and the `--json` plan envelope is a published
    /// contract that must not gain a key for it.
    #[serde(skip)]
    pub path_args: Vec<usize>,
    /// What exec does just before this step runs. Empty for every recipe
    /// that reads its inputs where they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepare: Vec<Prepare>,
}

/// Something exec does just before running a step, so the step finds
/// what it reads. Computed by the pure planner; performed by `exec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prepare {
    /// A file convkit writes.
    Write { path: PathBuf, contents: String },
    /// An input placed where the step reads it: a hard link to the input's
    /// resolved path, else a copy; with `strip`, a copy without its
    /// metadata (`jpeg::strip`).
    Stage {
        from: PathBuf,
        to: PathBuf,
        strip: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversionPlan {
    pub from: Format,
    pub to: Format,
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub steps: Vec<PlannedStep>,
    pub warnings: Vec<String>,
    /// Present only for a `--max-size` conversion. Skipped when absent so
    /// the published `--json` plan envelope is unchanged for every other
    /// conversion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizing: Option<crate::sized::SizingPlan>,
    /// What a `--resize --upscale` that enlarges the picture, or might,
    /// costs. Its warning prints as a warning, unlike `warnings`, which are
    /// notes; a large one is refused without consent. Skipped when absent,
    /// like `sizing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enlarged: Option<Enlargement>,
    /// The cut a `--start`/`--end`/`--duration` conversion made. Skipped
    /// when absent, like `sizing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<crate::trim::RangeReport>,
    /// The speed a `--speed` conversion plays at. Skipped when absent, like
    /// `sizing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<crate::speed::SpeedReport>,
}

/// Why a probe-aware media pair re-encoded instead of copying: a video knob.
const REENCODED_FOR_KNOB_NOTE: &str = "Re-encoded rather than stream-copied, because a video \
     knob changes the picture; the copy path cannot filter.";

/// Chooses a recipe and renders it with default tuning. Pure: no
/// filesystem, no process spawning, no executable resolution. The
/// canonical spelling for every caller that has no user tuning to apply;
/// `build_tuned` is the full entry point.
pub fn build(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    available: Option<&AvailableBackends>,
) -> Result<ConversionPlan> {
    build_tuned(
        from,
        to,
        inputs,
        output,
        probe,
        available,
        &Tuning::default(),
    )
}

/// `build` plus user tuning. A tuning flag whose slot the selected recipe
/// does not carry is a hard `InvalidInvocation` naming the flag and why —
/// never a silent no-op: a user who asked for `--resize` and got an
/// unresized file would have every right to stop trusting the tool.
pub fn build_tuned(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    available: Option<&AvailableBackends>,
    tuning: &Tuning,
) -> Result<ConversionPlan> {
    if inputs.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            "no input files were given",
        ));
    }

    // A password-protected .docx/.xlsx/.pptx is refused before LibreOffice
    // sees it: one LibreOffice does not recognise as encrypted is imported
    // as plain text, and the result is pages of the encrypted bytes with
    // an exit status of 0. An encrypted .doc/.xls/.ppt is still handed
    // over, because LibreOffice opens one encrypted with Office's built-in
    // default password; `exec::run` explains the failure if it does not.
    if probe.and_then(|p| p.office).is_some_and(|o| o.encrypted)
        && matches!(from, Format::Docx | Format::Xlsx | Format::Pptx)
    {
        return Err(ConvError::password_protected(&inputs[0], from));
    }

    // A size target is a policy, not a knob: it chooses the knob values
    // itself, so it takes the whole conversion (sized.rs).
    if let Some(max) = &tuning.max_size {
        return crate::sized::plan(from, to, inputs, output, probe, tuning, max);
    }

    // A file stripped into its own format: no registry pair covers it, so
    // it is planned here, as `--max-size` plans its own (above). A clip is
    // not a copy of the whole file, so a range takes the cut's path below,
    // which strips as it cuts, and so does a speed change.
    let timed = tuning.range.is_some() || tuning.speed.is_some();
    if from == to && tuning.strip_metadata && !timed {
        return in_place(from, inputs, output, probe, tuning);
    }

    // Resolved once, above every branch below, so the probe-aware dynamic
    // path and the static-recipe path both render against the same value
    // instead of the static path discarding it for `ResolvedVideo::default()`
    // (when it did, `--fps`/`--resize` on `video -> gif` and `gif -> mp4`
    // were accepted, exited 0, and did nothing, because neither pair ever
    // takes the dynamic branch below). An empty `Tuning`
    // resolves to `ResolvedVideo::default()` with no notes regardless of
    // `probe`, which is what keeps the untuned argv snapshot byte-identical.
    //
    // A range is resolved first, against the whole source, and everything
    // after it plans against the clip: the notes, the --upscale estimate.
    let clip = tuning
        .range
        .as_ref()
        .map(|r| crate::trim::clip(from, to, r, probe, &inputs[0]))
        .transpose()?;
    let probe = clip.as_ref().map(|c| &c.probe).or(probe);
    let cut = clip.as_ref().and_then(|c| c.cut);
    let range = clip.as_ref().map(|c| c.report.clone());
    let mut clip_notes = clip.as_ref().map(|c| c.notes.clone()).unwrap_or_default();
    // A speed change plays the clip, so it is resolved against the clip, and
    // everything after it plans against the output's length.
    let retimed = tuning
        .speed
        .map(|s| {
            crate::speed::retime(
                from,
                to,
                s,
                tuning.shift_pitch,
                tuning.mute,
                probe,
                &inputs[0],
            )
        })
        .transpose()?;
    let probe = retimed.as_ref().map(|r| &r.probe).or(probe);
    let speed = retimed.as_ref().map(|r| r.report.clone());
    clip_notes.extend(retimed.iter().flat_map(|r| r.notes.iter().cloned()));
    if tuning.mute && !crate::sized::is_video_target(to) {
        let why = if to == Format::Gif {
            "gif has no sound to drop".to_string()
        } else {
            format!("{} has no picture", to.ext())
        };
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("a silent clip needs a video target; {why}"),
        ));
    }
    let mut resolved = crate::video::resolve(tuning, probe, target_for(to));
    resolved.cut = cut;
    resolved.speed = retimed.as_ref().map(|r| r.retime.clone());
    resolved.mute = tuning.mute;
    resolved.ogg_tags = media::is_ogg(from) && !media::is_ogg(to);
    // An icon pair adds what it read of the source (`icon::resolve`); with
    // no probe, as in the snapshot, that is nothing.
    crate::icon::resolve(from, to, probe, &mut resolved);
    // A CSV source adds the import options read from it (`table::resolve`);
    // with no probe, as in the snapshot, it keeps the default.
    if from == Format::Csv {
        crate::table::resolve(probe, &mut resolved);
    }

    // Probe-aware media paths first: a container change whose video codec
    // already fits the target gets a stream-mapped copy (or hybrid
    // copy-video/transcode-audio) invocation built from the probe, and an
    // audio extraction whose codec the target holds natively gets a
    // `-c:a copy`. Both are constructed dynamically — the static registry
    // table can only spell literal argv, which is exactly how default
    // stream selection silently dropped tracks. Registration in the static
    // table still gates the pair (`lookup` below is what decides a pair is
    // supported at all); these only change *how* an already-supported pair
    // runs when a probe is in hand.
    if let Some(p) = probe {
        // A format cut or retimed into itself is no registered pair, so it
        // reaches the probe-aware paths by its range or speed instead.
        let same_format = from == to && timed;
        if same_format || (registry::lookup(from, to).is_some() && registry::needs_probe(from, to))
        {
            // A video knob changes the picture, and ffmpeg refuses a filter
            // alongside -c:v copy -- so consult the tuning before choosing
            // the path, not after. Choosing first is why `select()` below
            // used to be unreachable for every tuned media pair.
            //
            // Keyed on `resolved`, not `tuning`, for fps/scale: a `--fps`
            // that does not bind (the cap is above the source rate) resolves
            // to `None` and must fall through to the stream copy, not force
            // a re-encode that changes nothing but the file's size. `--crf`
            // stays a `tuning` check -- it retargets the encoder rather than
            // the filter chain, so `resolved` carries no signal for it, and
            // dropping this term would let `--crf` fall through to a copy
            // and silently discard the flag.
            //
            // A speed change filters the picture and the sound alike, so it
            // takes neither copy: an audio target that finds no transcode
            // here falls through to its static recipe, which filters. It is
            // never a copy, so its re-encode goes unexplained.
            let knob = resolved.fps.is_some() || resolved.scale.is_some() || tuning.crf.is_some();
            let wants_video = knob || resolved.speed.is_some();
            let strip = tuning.strip_metadata;
            // A copy can only make some cuts exactly (see
            // `trim::Cut::why_not_copied`); any other re-encodes the video to
            // cut where it was asked. Asked of the copy path itself, so the
            // note is only said where a copy was otherwise on offer.
            let not_copied = cut.and_then(|c| c.why_not_copied(p)).filter(|_| {
                media::stream_mapped_invocation(to, p, strip, &resolved, &inputs[0], output)
                    .is_some()
            });
            let dynamic = if wants_video || not_copied.is_some() {
                media::transcoded_invocation(
                    to, p, &resolved, tuning.crf, strip, &inputs[0], output,
                )
                .map(|mut m| {
                    let why = match (knob, resolved.speed.is_some(), not_copied) {
                        (true, _, _) => Some(REENCODED_FOR_KNOB_NOTE.to_string()),
                        (false, true, _) => None,
                        (false, false, why) => why.map(|why| {
                            format!("Re-encoded rather than stream-copied, because {why}")
                        }),
                    };
                    m.warnings.extend(why);
                    m
                })
            } else {
                media::stream_mapped_invocation(to, p, strip, &resolved, &inputs[0], output)
                    .or_else(|| {
                        media::audio_copy_invocation(
                            from,
                            to,
                            p,
                            strip,
                            cut.as_ref(),
                            &inputs[0],
                            output,
                        )
                    })
            };
            if let Some(mut m) = dynamic {
                validate_tuning_for_dynamic_media(from, to, tuning)?;
                m.warnings.extend(resolved.notes.iter().cloned());
                m.warnings.extend(clip_notes.iter().cloned());
                m.warnings
                    .extend(crate::metadata::location_note(from, to, probe, tuning));
                m.warnings
                    .extend(crate::metadata::labels_note(to, tuning, probe));
                // Every invocation `media.rs` builds opens with `-i <input>`,
                // after a cut's `-ss`/`-t` when there is one, and closes with
                // the output path: those are the path positions the Windows
                // long-path rewriter needs.
                let path_args = vec![media::input_position(&m.argv), m.argv.len() - 1];
                return Ok(ConversionPlan {
                    from,
                    to,
                    inputs: inputs.to_vec(),
                    output: output.to_path_buf(),
                    steps: vec![PlannedStep {
                        backend: Backend::Ffmpeg,
                        program: Backend::Ffmpeg.exe_name().to_string(),
                        argv: m.argv,
                        output_mode: OutputMode::Path,
                        output: output.to_path_buf(),
                        intermediate_ext: None,
                        path_args,
                        prepare: Vec::new(),
                    }],
                    warnings: m.warnings,
                    sizing: None,
                    enlarged: resolved.enlarged.clone(),
                    range,
                    speed,
                });
            }
        }
    }

    let recipe = select(from, to, probe, available)
        .or_else(|| {
            (from == to && timed)
                .then(|| registry::same_format_recipe(to))
                .flatten()
        })
        .ok_or_else(|| ConvError::unsupported_pair(from, to))?;
    let recipe = if tuning.strip_metadata {
        registry::strip_variant(to, recipe)
    } else {
        recipe
    };
    // A merge's pair is named by its first input alone, so whether Typst can
    // take it is decided here, over every input and the knobs.
    let recipe = registry::image_pdf_variant(recipe, inputs, tuning);
    validate_tuning(&recipe, from, to, tuning, &resolved)?;
    // The tags `--strip-metadata` writes back on an ffmpeg step.
    let kept = probe.map_or(&[][..], |p| p.kept_tags.as_slice());

    let mut steps = render_steps(&recipe, inputs, output, tuning, &resolved, kept);
    // Typst reads only under its root: the inputs are staged beside the
    // output, with the layout that places them.
    if recipe == registry::TYPST_IMG_TO_PDF {
        steps[0].prepare = crate::layout::image_pages(inputs, output, tuning.strip_metadata);
    }

    // Only the notes that apply to this source, as far as the probe knows.
    let mut warnings = registry::notes_for(&recipe, probe);
    warnings.extend(registry::gif_note(&recipe, inputs));
    // Mirrors the dynamic branch above (`m.warnings.extend(resolved.notes...)`)
    // -- a static recipe carrying an `Arg::VideoChain` slot gets the same
    // honesty about a cap that did not bind or a probe that never ran.
    warnings.extend(resolved.notes.iter().cloned());
    warnings.extend(clip_notes);
    warnings.extend(crate::metadata::tags_unread_note(from, to, tuning, probe));
    // A merge's later inputs carry their EXIF into the pdf too.
    let located = registry::location_source(&recipe, from, inputs);
    warnings.extend(crate::metadata::location_note(located, to, probe, tuning));
    warnings.extend(crate::metadata::labels_note(to, tuning, probe));

    Ok(ConversionPlan {
        from,
        to,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps,
        warnings,
        sizing: None,
        enlarged: resolved.enlarged.clone(),
        range,
        speed,
    })
}

/// Renders a static recipe's steps: each step's output (the final output, or
/// an intermediate named after it), its argv, and the positions of its
/// paths. Shared by `build_tuned` and `in_place`.
fn render_steps(
    recipe: &Recipe,
    inputs: &[PathBuf],
    output: &Path,
    tuning: &Tuning,
    resolved: &ResolvedVideo,
    kept: &[(String, String)],
) -> Vec<PlannedStep> {
    let last = recipe.steps.len() - 1;

    // Two passes: first compute every step's output path as an owned
    // `PathBuf`, then render each step's argv against `step_outputs`. A
    // single pass that pushes `PlannedStep`s while also holding a `&Path`
    // borrowed from the same growing `Vec` does not survive the borrow
    // checker.
    let mut step_outputs: Vec<PathBuf> = Vec::with_capacity(recipe.steps.len());
    for (i, step) in recipe.steps.iter().enumerate() {
        step_outputs.push(if i == last {
            output.to_path_buf()
        } else {
            let ext = step
                .intermediate_ext
                .expect("non-final step declares intermediate_ext");
            output.with_extension(format!("convkit-step{i}.{ext}"))
        });
    }

    let mut steps = Vec::with_capacity(recipe.steps.len());
    for (i, step) in recipe.steps.iter().enumerate() {
        let inputs_here: Vec<&Path> = if i == 0 {
            inputs.iter().map(PathBuf::as_path).collect()
        } else {
            vec![step_outputs[i - 1].as_path()]
        };
        let crate::recipe::Rendered {
            mut argv,
            mut path_args,
        } = step.render_full(&inputs_here, &step_outputs[i], tuning, resolved, kept);
        if step.backend == Backend::Soffice {
            // See `USER_INSTALLATION_PLACEHOLDER`'s docs: every real
            // Soffice invocation gets this flag from `exec::run`, so the
            // preview must show it too, at the same position (first),
            // rather than silently omitting a flag that's load-bearing for
            // profile isolation.
            argv.insert(0, USER_INSTALLATION_PLACEHOLDER.to_string());
            // Inserting at the front shifts every recorded position by one.
            for index in &mut path_args {
                *index += 1;
            }
        }
        steps.push(PlannedStep {
            backend: step.backend,
            program: step.backend.exe_name().to_string(),
            argv,
            output_mode: step.output,
            output: step_outputs[i].clone(),
            intermediate_ext: step.intermediate_ext.map(str::to_owned),
            path_args,
            prepare: Vec::new(),
        });
    }
    steps
}

/// `photo.jpg --strip-metadata` -> `photo-stripped.jpg`, and the same for
/// video and audio. An image runs the recipe any raster takes to its format
/// (through the TIFF for png), which re-encodes a lossy one, so that is
/// said. Video and audio are stream-copied with their tags cleared.
fn in_place(
    format: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    tuning: &Tuning,
) -> Result<ConversionPlan> {
    crate::metadata::in_place(format)
        .map_err(|why| ConvError::new(ErrorCode::InvalidInvocation, why))?;
    let kept = probe.map_or(&[][..], |p| p.kept_tags.as_slice());
    let (steps, mut warnings, enlarged) = match registry::image_recipe_for(format) {
        Some(recipe) => {
            let recipe = registry::strip_variant(format, recipe);
            // A compressed tiff stays compressed; only the read can tell.
            let compressed = probe.and_then(|p| p.image).is_some_and(|i| i.compressed);
            let recipe = if format == Format::Tiff && compressed {
                registry::IMG_TO_TIFF_ZIP
            } else {
                recipe
            };
            let resolved = crate::video::resolve(tuning, probe, target_for(format));
            validate_tuning(&recipe, format, format, tuning, &resolved)?;
            let steps = render_steps(&recipe, inputs, output, tuning, &resolved, kept);
            let mut warnings = registry::notes_for(&recipe, probe);
            if matches!(format, Format::Jpg | Format::Webp | Format::Avif) {
                warnings.push(crate::metadata::reencode_note(format, tuning));
            }
            (steps, warnings, resolved.enlarged)
        }
        None => {
            // A stream copy cannot filter or re-encode, so a video knob
            // would be silently ignored here; refuse it instead.
            let others = Tuning {
                strip_metadata: false,
                ..tuning.clone()
            };
            if !others.is_empty() {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "--strip-metadata on a {0} kept as {0} copies its streams, so no \
                         other tuning flag applies; add --to <format> to convert it",
                        format.ext()
                    ),
                ));
            }
            let m = media::same_format_copy(format, probe, &inputs[0], output);
            let path_args = vec![1, m.argv.len() - 1];
            let step = PlannedStep {
                backend: Backend::Ffmpeg,
                program: Backend::Ffmpeg.exe_name().to_string(),
                argv: m.argv,
                output_mode: OutputMode::Path,
                output: output.to_path_buf(),
                intermediate_ext: None,
                path_args,
                prepare: Vec::new(),
            };
            (vec![step], m.warnings, None)
        }
    };
    warnings.extend(crate::metadata::tags_unread_note(
        format, format, tuning, probe,
    ));
    warnings.extend(crate::metadata::labels_note(format, tuning, probe));
    Ok(ConversionPlan {
        from: format,
        to: format,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps,
        warnings,
        sizing: None,
        enlarged,
        range: None,
        speed: None,
    })
}

/// What kind of picture `to` is, for the notes and the size estimate a
/// `--upscale` warning gives.
fn target_for(to: Format) -> Target {
    if let (Format::Gif, ScaleStyle::CappedLanczos { default_width }) =
        (to, registry::TO_GIF_CHAIN.scale)
    {
        return Target::Gif {
            default_width: default_width
                .parse()
                .expect("an authored width is a number"),
            default_fps: registry::TO_GIF_CHAIN
                .fps
                .and_then(|f| f.parse().ok())
                .expect("GIF authors a whole frame rate"),
        };
    }
    if crate::sized::is_video_target(to) {
        return Target::Video;
    }
    // Bytes per pixel an enlarged picture came out at in each format,
    // measured with this tool on film stills and flat graphics at 4 and 16
    // times the pixels: wide on purpose, since content moves it as much as
    // size does.
    let bytes_per_pixel = match to {
        Format::Jpg => Some((0.05, 0.3)),
        Format::Webp => Some((0.02, 0.15)),
        Format::Avif => Some((0.01, 0.1)),
        Format::Png => Some((0.3, 2.0)),
        Format::Tiff => Some((1.0, 4.0)),
        Format::Bmp => Some((3.0, 4.0)),
        _ => None,
    };
    Target::Image { bytes_per_pixel }
}

/// Refuses the image knobs on the probe-selected media paths. The video
/// knobs are honoured there now (see `media::transcoded_invocation`), so
/// this is a per-flag table rather than the old three-way if/else whose
/// final `else` said "--colors" unconditionally -- a shape in which any new
/// field reported itself under the wrong name, with nothing at compile time
/// to object.
fn validate_tuning_for_dynamic_media(from: Format, to: Format, tuning: &Tuning) -> Result<()> {
    if tuning.is_empty() {
        return Ok(());
    }
    let image_only: [(&str, bool); 2] = [
        ("--quality", tuning.quality.is_some()),
        ("--colors", tuning.colors.is_some()),
    ];
    for (flag, given) in image_only {
        if given {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{flag} does not apply to {} -> {}: it tunes image conversions",
                    from.ext(),
                    to.ext(),
                ),
            ));
        }
    }
    // A real `--crf` reaching this far means `transcoded_invocation` already
    // accepted the pair -- this dynamic path has no slot check to make, only
    // the range convkit itself imposes.
    check_crf_range(to, tuning)
}

/// libx264 takes 0-51; libvpx-vp9 takes 0-63. The bound is convkit's own:
/// libx264 accepts far more than 51 without complaint. Shared by both
/// validators -- a probe-selected media pair reaches `transcoded_invocation`
/// (and so `validate_tuning_for_dynamic_media`) just as often as the static
/// table reaches `validate_tuning`, and the range is the same either way.
fn check_crf_range(to: Format, tuning: &Tuning) -> Result<()> {
    if let Some(n) = tuning.crf {
        if !matches!(to, Format::Webm) && n > 51 {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "--crf {n} is out of range for {}; libx264 takes 0-51, lower is better",
                    to.ext(),
                ),
            ));
        }
    }
    Ok(())
}

/// Refuses any tuning flag whose slot the selected recipe does not carry.
/// Per-flag, so `--quality` on a lossless target gets the honest "png is
/// lossless" answer while `--resize` on the same invocation still works.
///
/// `resolved` is the video knobs already resolved against the probe (or the
/// lack of one). It matters for exactly one pair shape, a probe-routed webm
/// target: `VIDEO_TO_WEBM` has no filter slot, so a knob can only be applied
/// by the probe-aware path in `build_tuned`, and this static recipe is
/// reached when that path declined. The question is then whether the knob
/// still needs applying, which only `resolved` answers.
fn validate_tuning(
    recipe: &Recipe,
    from: Format,
    to: Format,
    tuning: &Tuning,
    resolved: &ResolvedVideo,
) -> Result<()> {
    if tuning.is_empty() {
        return Ok(());
    }
    // An icon has no knob: its sizes are fixed, and it is lossless. Said
    // here, since the general refusals below would call ico something it
    // is not ("--resize ... tunes image, video and GIF targets").
    if to == Format::Ico {
        let flag = [
            ("--resize", tuning.resize.is_some()),
            ("--quality", tuning.quality.is_some()),
            ("--colors", tuning.colors.is_some()),
            ("--fps", tuning.fps.is_some()),
            ("--crf", tuning.crf.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag));
        if let Some(flag) = flag {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{flag} does not apply to {} -> ico: an icon holds fixed sizes, 16 to 256 px",
                    from.ext()
                ),
            ));
        }
    }
    let has_slot =
        |wanted: fn(&Arg) -> bool| recipe.steps.iter().any(|s| s.args.iter().any(&wanted));
    // On a probe-routed webm target, `--fps` and `--resize` are decided by
    // what they resolved to, never by the recipe's (absent) slot:
    //
    // - Something to apply (`resolved.fps` or `resolved.scale` is set) that
    //   this recipe cannot carry means the probe-aware path declined for
    //   want of a source to map: no probe ran, or it read no video stream
    //   (each knob resolves to a filter whenever the source is unknown or
    //   the cap binds). Say that, rather than the refusal below, which would
    //   claim webm is not a video target.
    // - Nothing to apply means the probe read the source and the cap does
    //   not bind (`--fps 60` on a 24 fps source, `--resize 4000x` on a
    //   1280x720 one): the flag is a no-op that `resolved.notes` already
    //   explains on the plan's warnings, so the checks below let it through
    //   rather than refusing it.
    let probe_routed_webm = to == Format::Webm && registry::needs_probe(from, to);
    if probe_routed_webm && (resolved.fps.is_some() || resolved.scale.is_some()) {
        let flag = if resolved.fps.is_some() {
            "--fps"
        } else {
            "--resize"
        };
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "{flag} on {} -> webm needs ffprobe to read the source, and it could not; \
                 check that ffprobe is installed and the input is a readable video",
                from.ext()
            ),
        ));
    }
    if tuning.resize.is_some()
        && !probe_routed_webm
        && !has_slot(|a| matches!(a, Arg::TuneResize | Arg::VideoChain(_)))
    {
        let (flags, verb) = if tuning.upscale {
            ("--resize and --upscale do", "they tune")
        } else {
            ("--resize does", "it tunes")
        };
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{flags} not apply to {} -> {}: {verb} image, video and GIF targets",
                from.ext(),
                to.ext(),
            ),
        ));
    }
    if tuning.quality.is_some() && !has_slot(|a| matches!(a, Arg::Quality(_))) {
        let why = if matches!(to, Format::Png | Format::Bmp | Format::Tiff) {
            format!(
                "{} is lossless; --quality applies to jpg/webp/avif targets and image -> pdf",
                to.ext()
            )
        } else {
            format!(
                "--quality does not apply to {} -> {}: it tunes lossy image targets and image -> pdf",
                from.ext(),
                to.ext(),
            )
        };
        return Err(ConvError::new(ErrorCode::InvalidInvocation, why));
    }
    if tuning.colors.is_some() && !has_slot(|a| matches!(a, Arg::TuneColors)) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--colors does not apply to {} -> {}: it tunes raster image targets",
                from.ext(),
                to.ext(),
            ),
        ));
    }
    if tuning.fps.is_some() && !probe_routed_webm && !has_slot(|a| matches!(a, Arg::VideoChain(_)))
    {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--fps does not apply to {} -> {}: it tunes video and GIF targets",
                from.ext(),
                to.ext(),
            ),
        ));
    }
    if tuning.crf.is_some() && !has_slot(|a| matches!(a, Arg::Crf(_))) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--crf does not apply to {} -> {}: it tunes video targets",
                from.ext(),
                to.ext(),
            ),
        ));
    }
    if tuning.strip_metadata && !has_slot(|a| matches!(a, Arg::StripMetadata)) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--strip-metadata does not apply to {} -> {}: it covers image, video and \
                 audio conversions",
                from.ext(),
                to.ext(),
            ),
        ));
    }
    check_crf_range(to, tuning)
}

/// Chooses among the *static* recipes; the probe-aware stream-mapping
/// paths are handled in `build` itself (see `media`), so by the time this
/// runs the remaining probe question is only which static sibling fits —
/// today, whether a gif target needs the tonemapping chain for an HDR
/// source (`registry::gif_recipe_for`).
///
/// `available` picks between the canonical (soffice) and fallback
/// (pandoc+typst) recipes for a pair that `registry::has_fallback` — today,
/// only `docx`/`odt` → `pdf`. `None` (the caller has no availability
/// information, or never bothered to check because the pair has no
/// fallback anyway) always yields the canonical recipe, keeping the
/// argv snapshot (`recipes.rs`'s `every_registered_pair_renders_stable_argv`)
/// byte-identical to before this existed. `Some` prefers soffice when
/// present; otherwise pandoc+typst when *both* are present; otherwise falls
/// through to the canonical (soffice) recipe anyway, so a user with neither
/// route available gets the ordinary `backend_missing` naming soffice —
/// the pair's own primary backend — rather than a confusing one naming
/// typst.
fn select(
    from: Format,
    to: Format,
    probe: Option<&MediaProbe>,
    available: Option<&AvailableBackends>,
) -> Option<Recipe> {
    if let Some(p) = probe {
        if let Some(r) = registry::probe_selected(from, to, p) {
            return Some(r);
        }
    }

    if let Some(avail) = available {
        if !avail.has(Backend::Soffice) && avail.has(Backend::Pandoc) && avail.has(Backend::Typst) {
            if let Some(fallback) = registry::lookup_fallback(from, to) {
                return Some(fallback);
            }
        }
    }

    registry::lookup(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MediaProbe;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn strip_metadata_is_refused_on_a_document_pair() {
        let e = build_tuned(
            Format::Docx,
            Format::Pdf,
            &[p("a.docx")],
            Path::new("a.pdf"),
            None,
            None,
            &Tuning {
                strip_metadata: true,
                ..Tuning::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--strip-metadata does not apply to docx -> pdf: it covers image, video and \
             audio conversions"
        );
    }

    /// The flag swaps a png target onto its TIFF detour, and nothing else
    /// about the plan.
    #[test]
    fn a_stripped_png_plan_writes_a_tiff_first() {
        let plan = build_tuned(
            Format::Heic,
            Format::Png,
            &[p("a.heic")],
            Path::new("a.png"),
            None,
            None,
            &Tuning {
                strip_metadata: true,
                ..Tuning::default()
            },
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].output, Path::new("a.convkit-step0.tiff"));
        assert_eq!(plan.steps[1].argv, ["a.convkit-step0.tiff", "a.png"]);
    }

    fn merge(tuning: &Tuning) -> Result<ConversionPlan> {
        build_tuned(
            Format::Jpg,
            Format::Pdf,
            &[p("x/a.jpg"), p("y/b.png")],
            Path::new("d/out.pdf"),
            None,
            None,
            tuning,
        )
    }

    /// Typst reads only under its root, so the inputs are staged beside the
    /// output (the scratch dir in a real run) with the layout naming them.
    #[test]
    fn an_image_merge_plans_one_typst_step() {
        let plan = merge(&Tuning::default()).unwrap();
        assert_eq!(plan.steps.len(), 1);
        let step = &plan.steps[0];
        assert_eq!(step.backend, Backend::Typst);
        assert_eq!(
            step.argv,
            [
                "compile",
                "--root",
                "d",
                "--ignore-system-fonts",
                "--diagnostic-format",
                "short",
                "d/out.convkit-layout.typ",
                "d/out.pdf"
            ]
        );
        assert_eq!(step.path_args, [2, 6, 7], "the root, layout and output");
        assert_eq!(
            step.prepare,
            crate::layout::image_pages(
                &[p("x/a.jpg"), p("y/b.png")],
                Path::new("d/out.pdf"),
                false
            )
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_merge_with_quality_plans_the_magick_recipe_as_before() {
        let plan = merge(&Tuning {
            quality: Some(80),
            ..Tuning::default()
        })
        .unwrap();
        assert_eq!(plan.steps.len(), 1);
        let step = &plan.steps[0];
        assert_eq!(step.backend, Backend::Magick);
        assert_eq!(
            step.argv,
            [
                "x/a.jpg",
                "y/b.png",
                "-auto-orient",
                "-compress",
                "jpeg",
                "-quality",
                "80",
                "d/out.pdf"
            ]
        );
        assert!(step.prepare.is_empty());
    }

    /// An animated picture gives its first frame on either route, so the
    /// flag that moves a merge to ImageMagick never changes its page count.
    /// A TIFF keeps every page, as a scan does.
    #[test]
    fn a_magick_merge_takes_the_first_frame_of_a_gif_or_webp() {
        let plan = build_tuned(
            Format::Jpg,
            Format::Pdf,
            &[p("a.jpg"), p("b.gif"), p("c.webp"), p("d.tiff")],
            Path::new("out.pdf"),
            None,
            None,
            &Tuning {
                quality: Some(80),
                ..Tuning::default()
            },
        )
        .unwrap();
        let step = &plan.steps[0];
        assert_eq!(step.backend, Backend::Magick);
        assert_eq!(
            step.argv,
            [
                "a.jpg",
                "b.gif[0]",
                "c.webp[0]",
                "d.tiff",
                "-auto-orient",
                "-compress",
                "jpeg",
                "-quality",
                "80",
                "out.pdf"
            ]
        );
        assert_eq!(step.path_args, [0, 1, 2, 3, 9], "each input and the output");
    }

    #[test]
    fn strip_metadata_is_accepted_on_the_typst_route() {
        let plain = merge(&Tuning::default()).unwrap();
        let plan = merge(&Tuning {
            strip_metadata: true,
            ..Tuning::default()
        })
        .unwrap();
        let step = &plan.steps[0];
        assert_eq!(step.backend, Backend::Typst);
        assert_eq!(step.argv, plain.steps[0].argv, "the slot renders nothing");
        let strips: Vec<bool> = step
            .prepare
            .iter()
            .filter_map(|prep| match prep {
                Prepare::Stage { strip, .. } => Some(*strip),
                Prepare::Write { .. } => None,
            })
            .collect();
        assert_eq!(strips, [true, false], "the jpg is stripped, the png linked");
    }

    fn pdf_of(
        from: Format,
        inputs: &[&str],
        probe: Option<&MediaProbe>,
        t: &Tuning,
    ) -> ConversionPlan {
        let inputs: Vec<PathBuf> = inputs.iter().map(|s| p(s)).collect();
        build_tuned(
            from,
            Format::Pdf,
            &inputs,
            Path::new("out.pdf"),
            probe,
            None,
            t,
        )
        .unwrap()
    }

    /// A GIF gives a merge its first frame, on either route, and says so.
    #[test]
    fn a_gif_in_a_typst_merge_gets_the_first_frame_note() {
        let plain = Tuning::default();
        let note = [registry::GIF_FIRST_FRAME_NOTE];
        assert_eq!(
            pdf_of(Format::Jpg, &["a.jpg", "b.gif"], None, &plain).warnings,
            note
        );
        assert_eq!(pdf_of(Format::Gif, &["a.GIF"], None, &plain).warnings, note);
        let quality = Tuning {
            quality: Some(80),
            ..Tuning::default()
        };
        let magick = pdf_of(Format::Jpg, &["a.jpg", "b.gif"], None, &quality);
        assert_eq!(magick.steps[0].backend, Backend::Magick);
        assert_eq!(magick.warnings, note);
        let heic = pdf_of(Format::Jpg, &["a.jpg", "b.gif", "c.heic"], None, &plain);
        assert_eq!(heic.warnings, note);
        // A WebP is not singled out: the note would sit on every still one.
        let webp = pdf_of(Format::Jpg, &["a.jpg", "b.webp"], None, &plain);
        assert!(webp.warnings.is_empty(), "{:?}", webp.warnings);
    }

    const PDF_LOCATION_NOTE: &str = "The source records a GPS location, and the pdf keeps \
         it; add --strip-metadata to remove it.";

    /// Typst embeds a JPEG as it is, so its EXIF reaches the PDF, while a
    /// PNG, WebP or GIF is decoded to pixels. The location is read from the
    /// JPEGs, wherever they are in the merge, without ImageMagick.
    #[test]
    fn the_location_note_comes_from_a_jpeg_anywhere_in_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let (gif, jpg) = (dir.path().join("a.gif"), dir.path().join("gps.jpg"));
        std::fs::write(&gif, b"GIF89a").unwrap();
        std::fs::write(&jpg, crate::jpeg::located_for_tests()).unwrap();
        let inputs = [gif, jpg.clone()];
        let plain = Tuning::default();
        let traits = registry::native_image_traits(Format::Gif, Format::Pdf, &inputs, &plain);
        assert!(traits.is_some_and(|t| t.location), "{traits:?}");
        let probe = MediaProbe {
            image: traits,
            ..MediaProbe::default()
        };
        let names: Vec<&str> = inputs.iter().map(|i| i.to_str().unwrap()).collect();
        let plan = pdf_of(Format::Gif, &names, Some(&probe), &plain);
        assert_eq!(
            plan.warnings,
            [registry::GIF_FIRST_FRAME_NOTE, PDF_LOCATION_NOTE]
        );

        // The flag takes the location out, so there is nothing to read.
        let strip = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        let traits = registry::native_image_traits(Format::Gif, Format::Pdf, &inputs, &strip);
        assert!(traits.is_some_and(|t| !t.location), "{traits:?}");

        std::fs::write(&jpg, b"\xff\xd8\xff\xd9").unwrap();
        let traits = registry::native_image_traits(Format::Gif, Format::Pdf, &inputs, &plain);
        assert!(traits.is_some_and(|t| !t.location), "{traits:?}");
        // Off the Typst route ImageMagick reads the source, as before.
        let quality = Tuning {
            quality: Some(80),
            ..Tuning::default()
        };
        assert_eq!(
            registry::native_image_traits(Format::Gif, Format::Pdf, &inputs, &quality),
            None
        );
        assert_eq!(
            registry::native_image_traits(Format::Jpg, Format::Png, &inputs[1..], &plain),
            None
        );
    }

    /// ImageMagick carries every input's EXIF into the pdf, so a JPEG's
    /// location is noted whatever input comes first, a GIF or a WebP
    /// included.
    #[test]
    fn an_imagemagick_merge_notes_a_located_jpeg_after_a_gif_or_webp() {
        let located = MediaProbe {
            image: Some(crate::probe::ImageTraits {
                alpha: None,
                multi_frame: false,
                location: true,
                compressed: false,
            }),
            ..MediaProbe::default()
        };
        let quality = Tuning {
            quality: Some(80),
            ..Tuning::default()
        };
        let gif = pdf_of(Format::Gif, &["a.gif", "gps.jpg"], Some(&located), &quality);
        assert_eq!(gif.steps[0].backend, Backend::Magick);
        assert_eq!(
            gif.warnings,
            [registry::GIF_FIRST_FRAME_NOTE, PDF_LOCATION_NOTE]
        );
        let webp = pdf_of(
            Format::Webp,
            &["a.webp", "gps.jpg"],
            Some(&located),
            &quality,
        );
        assert_eq!(webp.warnings, [PDF_LOCATION_NOTE]);
        let heic = pdf_of(
            Format::Gif,
            &["a.gif", "gps.jpg", "x.heic"],
            Some(&located),
            &Tuning::default(),
        );
        assert_eq!(
            heic.warnings,
            [registry::GIF_FIRST_FRAME_NOTE, PDF_LOCATION_NOTE]
        );
    }

    #[test]
    fn no_location_note_with_strip_metadata() {
        let located = MediaProbe {
            image: Some(crate::probe::ImageTraits {
                alpha: None,
                multi_frame: false,
                location: true,
                compressed: false,
            }),
            ..MediaProbe::default()
        };
        let plain = pdf_of(
            Format::Png,
            &["a.png", "b.jpg"],
            Some(&located),
            &Tuning::default(),
        );
        assert_eq!(plain.warnings, [PDF_LOCATION_NOTE]);
        let strip = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        let stripped = pdf_of(Format::Png, &["a.png", "b.jpg"], Some(&located), &strip);
        assert!(stripped.warnings.is_empty(), "{:?}", stripped.warnings);
    }

    /// With no probe there is nothing to write back, so every tag goes and
    /// the note says title and artist went with them.
    #[test]
    fn without_a_probe_a_stripped_audio_conversion_says_its_tags_went_too() {
        let t = Tuning {
            strip_metadata: true,
            ..Tuning::default()
        };
        let plan = build_tuned(
            Format::Flac,
            Format::Mp3,
            &[p("a.flac")],
            Path::new("a.mp3"),
            None,
            None,
            &t,
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-map_metadata", "-1"]),
            "{argv:?}"
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("title and artist")),
            "{:?}",
            plan.warnings
        );
        // A gif holds no tags, so there is nothing to say.
        let gif = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("a.mp4")],
            Path::new("a.gif"),
            None,
            None,
            &t,
        )
        .unwrap();
        assert!(gif.warnings.iter().all(|w| !w.contains("title")));
    }

    /// A static recipe writes back what the probe read.
    #[test]
    fn a_stripped_transcode_writes_back_the_kept_tags() {
        let probe = MediaProbe {
            audio_codecs: vec!["flac".into()],
            kept_tags: vec![("title".into(), "Song".into())],
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Flac,
            Format::Mp3,
            &[p("a.flac")],
            Path::new("a.mp3"),
            Some(&probe),
            None,
            &Tuning {
                strip_metadata: true,
                ..Tuning::default()
            },
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-metadata", "title=Song"]),
            "{argv:?}"
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    /// An Ogg source's tags are on its audio stream, so they are mapped to
    /// the file's own for a target that is not Ogg, and only the kept ones
    /// are written back when the flag clears them.
    #[test]
    fn an_ogg_sources_tags_reach_a_target_that_is_not_ogg() {
        let probe = MediaProbe {
            audio_codecs: vec!["opus".into()],
            kept_tags: vec![("title".into(), "Song".into())],
            ..MediaProbe::default()
        };
        let stream_tags = ["-map_metadata", "0:s:a:0"];
        let plan_for = |to: Format, strip_metadata: bool| {
            let t = Tuning {
                strip_metadata,
                ..Tuning::default()
            };
            let out = format!("a.{}", to.ext());
            build_tuned(
                Format::Opus,
                to,
                &[p("a.opus")],
                Path::new(&out),
                Some(&probe),
                None,
                &t,
            )
            .unwrap()
            .steps[0]
                .argv
                .clone()
        };
        for to in [Format::Mp3, Format::M4a, Format::Flac, Format::Wav] {
            let argv = plan_for(to, false);
            assert!(argv.windows(2).any(|w| w == stream_tags), "{argv:?}");
            let argv = plan_for(to, true);
            assert!(!argv.windows(2).any(|w| w == stream_tags), "{argv:?}");
            assert!(
                argv.windows(2).any(|w| w == ["-metadata", "title=Song"]),
                "{argv:?}"
            );
        }
        // Into Ogg, the stream's tags are carried as they are.
        let argv = plan_for(Format::Ogg, false);
        assert!(!argv.iter().any(|a| a == "-map_metadata"), "{argv:?}");
    }

    #[test]
    fn unsupported_pair_is_an_error_not_a_panic() {
        let e = build(
            Format::Pdf,
            Format::Mp4,
            &[p("in.pdf")],
            Path::new("out.mp4"),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::UnsupportedPair);
    }

    #[test]
    fn compatible_codecs_select_the_remux_recipe() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-map", "0:v:0"]),
            "the remux must map its streams explicitly, never rely on default selection: {:?}",
            plan.steps[0].argv
        );
    }

    #[test]
    fn incompatible_codecs_fall_back_to_transcoding() {
        let probe = MediaProbe {
            video_codec: Some("vp9".into()),
            audio_codecs: vec!["opus".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.contains(&"libx264".to_string()),
            "{:?}",
            plan.steps[0].argv
        );
    }

    #[test]
    fn a_missing_probe_conservatively_transcodes() {
        let plan = build(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            None,
            None,
        )
        .unwrap();
        assert!(plan.steps[0].argv.contains(&"libx264".to_string()));
    }

    #[test]
    fn program_is_the_bare_exe_name_so_snapshots_are_machine_independent() {
        let plan = build(
            Format::Heic,
            Format::Jpg,
            &[p("a.heic")],
            Path::new("b.jpg"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps[0].program, "magick");
    }

    /// A password-protected .docx/.xlsx/.pptx is refused while planning, so
    /// `--dry-run` says so too. A legacy one is planned: LibreOffice opens
    /// files encrypted with Office's default password.
    #[test]
    fn a_password_protected_ooxml_source_is_refused_and_a_legacy_one_is_not() {
        let locked = MediaProbe {
            office: Some(crate::office::OfficeTraits {
                encrypted: true,
                macros: false,
            }),
            ..MediaProbe::default()
        };
        for from in [Format::Docx, Format::Xlsx, Format::Pptx] {
            let input = p(&format!("in.{}", from.ext()));
            let e = build(
                from,
                Format::Pdf,
                &[input],
                Path::new("out.pdf"),
                Some(&locked),
                None,
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::PasswordProtected, "{from:?}");
        }
        for from in [Format::Doc, Format::Xls, Format::Ppt] {
            let input = p(&format!("in.{}", from.ext()));
            build(
                from,
                Format::Pdf,
                &[input],
                Path::new("out.pdf"),
                Some(&locked),
                None,
            )
            .unwrap_or_else(|e| panic!("{from:?}: {e}"));
        }
    }

    /// A CSV is imported with the options read from it; unread, with the
    /// default shape, which is what the snapshot shows.
    #[test]
    fn a_csv_is_imported_with_the_options_read_from_it() {
        let read = MediaProbe {
            table: Some(crate::table::TableShape::Csv(crate::table::CsvShape {
                delimiter: b';',
                text_columns: vec![(2, "zip".into())],
                ..crate::table::CsvShape::default()
            })),
            ..MediaProbe::default()
        };
        let infilter = |probe: Option<&MediaProbe>| {
            let plan = build(
                Format::Csv,
                Format::Xlsx,
                &[p("in.csv")],
                Path::new("out.xlsx"),
                probe,
                None,
            )
            .unwrap();
            plan.steps[0]
                .argv
                .iter()
                .find(|a| a.starts_with("--infilter="))
                .cloned()
                .unwrap()
        };
        assert!(
            infilter(Some(&read)).contains(":59,34,76,1,2/2,1033,"),
            "{}",
            infilter(Some(&read))
        );
        assert!(infilter(None).contains(":44,34,76,1,,1033,"));
    }

    #[test]
    fn warnings_travel_on_the_plan() {
        let plan = build(
            Format::Pdf,
            Format::Docx,
            &[p("a.pdf")],
            Path::new("b.docx"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1);
    }

    fn stripped() -> Tuning {
        Tuning {
            strip_metadata: true,
            ..Tuning::default()
        }
    }

    /// A lossy image stripped into its own format is re-encoded, and says
    /// so; a lossless one is not worth a note. png still takes its TIFF
    /// detour.
    #[test]
    fn an_image_stripped_in_place_is_re_encoded_and_says_so_when_lossy() {
        let plan = build_tuned(
            Format::Jpg,
            Format::Jpg,
            &[p("a.jpg")],
            Path::new("a-stripped.jpg"),
            None,
            None,
            &stripped(),
        )
        .unwrap();
        assert!(plan.steps[0].argv.contains(&"+profile".to_string()));
        assert!(
            plan.warnings.iter().any(|w| w
                == "The jpg is re-encoded at quality 92 to remove its metadata; ImageMagick \
                    cannot take it out of a jpg without re-encoding."),
            "{:?}",
            plan.warnings
        );
        let q70 = Tuning {
            quality: Some(70),
            ..stripped()
        };
        let webp = build_tuned(
            Format::Webp,
            Format::Webp,
            &[p("a.webp")],
            Path::new("a-stripped.webp"),
            None,
            None,
            &q70,
        )
        .unwrap();
        assert!(webp.warnings.iter().any(|w| w.contains("quality 70")));
        let png = build_tuned(
            Format::Png,
            Format::Png,
            &[p("a.png")],
            Path::new("a-stripped.png"),
            None,
            None,
            &stripped(),
        )
        .unwrap();
        assert_eq!(png.steps.len(), 2);
        assert!(png.warnings.iter().all(|w| !w.contains("re-encoded")));
    }

    /// ImageMagick writes a tiff uncompressed unless told otherwise, so a
    /// compressed tiff stripped into a tiff is written with lossless Zip,
    /// and an uncompressed one, or one not read, stays as it is.
    #[test]
    fn a_compressed_tiff_stripped_in_place_stays_compressed() {
        let read = |compressed| MediaProbe {
            image: Some(crate::probe::ImageTraits {
                alpha: None,
                multi_frame: false,
                location: false,
                compressed,
            }),
            ..MediaProbe::default()
        };
        let plan_for = |probe: Option<&MediaProbe>| {
            build_tuned(
                Format::Tiff,
                Format::Tiff,
                &[p("scan.tiff")],
                Path::new("scan-stripped.tiff"),
                probe,
                None,
                &stripped(),
            )
            .unwrap()
        };
        let zipped = |plan: &ConversionPlan| {
            plan.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-compress", "zip"])
        };
        let plan = plan_for(Some(&read(true)));
        assert!(zipped(&plan), "{:?}", plan.steps[0].argv);
        assert_eq!(plan.steps[0].argv.last().unwrap(), "scan-stripped.tiff");
        assert!(!zipped(&plan_for(Some(&read(false)))));
        assert!(!zipped(&plan_for(None)));
    }

    /// Every stream is copied but the data tracks, which can carry a
    /// location of their own; the tags are cleared and the kept ones
    /// written back.
    #[test]
    fn a_video_stripped_in_place_is_a_stream_copy() {
        let probe = MediaProbe {
            video_codec: Some("hevc".into()),
            audio_codecs: vec!["aac".into()],
            kept_tags: vec![("title".into(), "Clip".into())],
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mov,
            Format::Mov,
            &[p("a.mov")],
            Path::new("a-stripped.mov"),
            Some(&probe),
            None,
            &stripped(),
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        let has = |pair: [&str; 2]| argv.windows(2).any(|w| w == pair);
        assert!(has(["-c", "copy"]), "{argv:?}");
        assert!(has(["-map", "-0:d"]), "{argv:?}");
        assert!(has(["-map_metadata", "-1"]), "{argv:?}");
        assert!(has(["-metadata", "title=Clip"]), "{argv:?}");
        assert_eq!(plan.steps[0].path_args, [1, argv.len() - 1]);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let unread = build_tuned(
            Format::Mp3,
            Format::Mp3,
            &[p("a.mp3")],
            Path::new("a-stripped.mp3"),
            None,
            None,
            &stripped(),
        )
        .unwrap();
        assert!(unread
            .warnings
            .iter()
            .any(|w| w.contains("title and artist")));
    }

    /// A stream copy cannot honour a video knob, so one is refused rather
    /// than ignored; an image kept in its format still takes its knobs.
    #[test]
    fn a_same_format_video_copy_refuses_a_knob_it_cannot_apply() {
        let t = Tuning {
            fps: Some("24".into()),
            ..stripped()
        };
        let e = build_tuned(
            Format::Mp4,
            Format::Mp4,
            &[p("a.mp4")],
            Path::new("a-stripped.mp4"),
            None,
            None,
            &t,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("add --to <format>"), "{}", e.message);
        let resized = Tuning {
            resize: Some("50%".into()),
            ..stripped()
        };
        let plan = build_tuned(
            Format::Jpg,
            Format::Jpg,
            &[p("a.jpg")],
            Path::new("a-stripped.jpg"),
            None,
            None,
            &resized,
        )
        .unwrap();
        assert!(plan.steps[0].argv.contains(&"50%>".to_string()));
    }

    /// conv cannot write heic, and without the flag a same-format pair is
    /// still no pair at all.
    #[test]
    fn only_the_flag_and_a_writable_format_strip_in_place() {
        let e = build_tuned(
            Format::Heic,
            Format::Heic,
            &[p("a.heic")],
            Path::new("b.heic"),
            None,
            None,
            &stripped(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "conv cannot write heic; add --to jpg to strip it into a jpg"
        );
        let e = build_tuned(
            Format::Docx,
            Format::Docx,
            &[p("a.docx")],
            Path::new("b.docx"),
            None,
            None,
            &stripped(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        let e = build_tuned(
            Format::Jpg,
            Format::Jpg,
            &[p("a.jpg")],
            Path::new("b.jpg"),
            None,
            None,
            &Tuning::default(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
    }

    /// The plan carries only the notes that apply to the source the probe
    /// read, and its argv does not change with them.
    #[test]
    fn a_plan_carries_only_the_notes_its_source_needs() {
        let photo = MediaProbe {
            image: Some(crate::probe::ImageTraits {
                alpha: Some(false),
                multi_frame: false,
                location: false,
                compressed: false,
            }),
            ..MediaProbe::default()
        };
        let plan_for = |probe: Option<&MediaProbe>| {
            build(
                Format::Heic,
                Format::Jpg,
                &[p("a.heic")],
                Path::new("a.jpg"),
                probe,
                None,
            )
            .unwrap()
        };
        let (read, unread) = (plan_for(Some(&photo)), plan_for(None));
        assert!(read.warnings.is_empty(), "{:?}", read.warnings);
        assert_eq!(unread.warnings.len(), 1, "{:?}", unread.warnings);
        assert_eq!(read.steps, unread.steps);

        let clip = MediaProbe {
            video_codec: Some("h264".into()),
            duration_ms: Some(2_000),
            ..MediaProbe::default()
        };
        let gif = build(
            Format::Mp4,
            Format::Gif,
            &[p("a.mp4")],
            Path::new("a.gif"),
            Some(&clip),
            None,
        )
        .unwrap();
        assert!(gif.warnings.is_empty(), "{:?}", gif.warnings);
    }

    /// An icon is squared at its source's own size, warns about the sizes
    /// it enlarges without asking, and reads an .ico's largest frame.
    #[test]
    fn an_icon_plan_follows_what_was_read_of_the_source() {
        let read = |w, h| MediaProbe {
            width: Some(w),
            height: Some(h),
            ..MediaProbe::default()
        };
        let icon = |probe: &MediaProbe| {
            build(
                Format::Png,
                Format::Ico,
                &[p("in.png")],
                Path::new("out.ico"),
                Some(probe),
                None,
            )
            .unwrap()
        };
        let banner = icon(&read(1200, 630));
        assert!(
            banner.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-extent", "1200x1200"]),
            "{:?}",
            banner.steps[0].argv
        );
        assert_eq!(banner.enlarged, None);
        let small = icon(&read(32, 32));
        let e = small.enlarged.expect("32 px is smaller than four sizes");
        assert!(!e.needs_confirmation);

        let favicon = MediaProbe {
            frames: vec![(16, 16, 8), (32, 32, 8), (48, 48, 8)],
            ..MediaProbe::default()
        };
        let back = build(
            Format::Ico,
            Format::Png,
            &[p("favicon.ico")],
            Path::new("favicon.png"),
            Some(&favicon),
            None,
        )
        .unwrap();
        assert_eq!(back.steps[0].argv[0], "favicon.ico[2]");
    }

    #[test]
    fn a_knob_on_an_icon_is_refused_with_the_reason() {
        for tuning in [
            Tuning {
                resize: Some("64x64".into()),
                ..Tuning::default()
            },
            Tuning {
                quality: Some(80),
                ..Tuning::default()
            },
            Tuning {
                colors: Some(16),
                ..Tuning::default()
            },
        ] {
            let e = build_tuned(
                Format::Png,
                Format::Ico,
                &[p("in.png")],
                Path::new("out.ico"),
                None,
                None,
                &tuning,
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation);
            assert!(
                e.message
                    .ends_with("an icon holds fixed sizes, 16 to 256 px"),
                "{}",
                e.message
            );
        }
    }

    #[test]
    fn intermediate_steps_write_to_the_declared_extension() {
        let plan = build(
            Format::Md,
            Format::Pdf,
            &[p("a.md")],
            Path::new("out/b.pdf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].intermediate_ext.as_deref(), Some("docx"));
        assert!(
            plan.steps[0].argv.last().unwrap().ends_with(".docx"),
            "{:?}",
            plan.steps[0].argv
        );
    }

    // --- Each target container gets its own remux variant ------------------

    /// `-movflags +faststart` is an mp4-muxer-only option that makes ffmpeg
    /// exit 1 on a WebM output, so the stream-mapped webm invocation must
    /// never carry it.
    #[test]
    fn mkv_to_webm_with_compatible_codecs_selects_the_webm_remux_variant() {
        let probe = MediaProbe {
            video_codec: Some("vp9".into()),
            audio_codecs: vec!["opus".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mkv,
            Format::Webm,
            &[p("in.mkv")],
            Path::new("out.webm"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"])
                && plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            !plan.steps[0].argv.contains(&"-movflags".to_string()),
            "webm remux must not carry the mp4-only -movflags option: {:?}",
            plan.steps[0].argv
        );
    }

    /// mov/mkv as conversion targets: `mp4 -> mov` with compatible codecs
    /// must produce an mp4-muxer-family stream copy carrying
    /// `-movflags +faststart`.
    #[test]
    fn mp4_to_mov_with_compatible_codecs_selects_the_mov_remux_variant() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mp4,
            Format::Mov,
            &[p("in.mp4")],
            Path::new("out.mov"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"])
                && plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.contains(&"-movflags".to_string()),
            "mov shares the mp4 muxer family, so its remux must keep +faststart: {:?}",
            plan.steps[0].argv
        );
    }

    /// `mp4 -> mkv` with compatible codecs must produce a keep-everything
    /// stream copy with no `-movflags` at all (matroska is not part of the
    /// mov/mp4 muxer family).
    #[test]
    fn mp4_to_mkv_with_compatible_codecs_selects_the_mkv_remux_variant_with_no_movflags() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mp4,
            Format::Mkv,
            &[p("in.mp4")],
            Path::new("out.mkv"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"])
                && plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-map", "0"]),
            "mkv remux must preserve every stream via -map 0: {:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-map", "-0:d"]),
            "mkv remux must exclude data streams the matroska muxer rejects: {:?}",
            plan.steps[0].argv
        );
        assert!(
            !plan.steps[0].argv.contains(&"-movflags".to_string()),
            "mkv is not part of the mov/mp4 muxer family and must never carry -movflags: {:?}",
            plan.steps[0].argv
        );
    }

    /// A real `mp4` source's *only* possible subtitle codec is `mov_text`,
    /// and matroska has no codec ID for it, so a plain keep-everything copy
    /// would make `mp4 -> mkv` fail outright on any source carrying a
    /// subtitle track. The stream-mapped invocation must keep video/audio
    /// as a copy and re-encode only the subtitle to SRT.
    #[test]
    fn mp4_to_mkv_with_a_mov_text_subtitle_reencodes_only_the_subtitle_stream() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            subtitle_codecs: vec!["mov_text".into()],
            ..MediaProbe::default()
        };
        let plan = build(
            Format::Mp4,
            Format::Mkv,
            &[p("in.mp4")],
            Path::new("out.mkv"),
            Some(&probe),
            None,
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:s", "srt"]),
            "{:?}",
            plan.steps[0].argv
        );
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(plan.warnings[0].contains("mov_text"), "{:?}", plan.warnings);
    }

    /// `Arg::Input` indexes `inputs[0]` unchecked in `Step::render`, so an
    /// empty slice must be rejected here at the public entry point, before
    /// any `Step` is ever rendered.
    #[test]
    fn empty_inputs_is_rejected_rather_than_panicking() {
        let e = build(
            Format::Heic,
            Format::Jpg,
            &[],
            Path::new("b.jpg"),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InputNotFound);
    }

    /// `PlannedStep::output` is the path that step actually writes; the
    /// execution engine needs this because reading the last argv
    /// element is wrong for `soffice` recipes, whose argv ends with the
    /// *input* path.
    #[test]
    fn planned_step_output_is_the_path_that_step_writes() {
        let plan = build(
            Format::Md,
            Format::Pdf,
            &[p("a.md")],
            Path::new("out/b.pdf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert!(
            plan.steps[0].output.to_string_lossy().ends_with(".docx"),
            "{:?}",
            plan.steps[0].output
        );
        assert_eq!(plan.steps[1].output, PathBuf::from("out/b.pdf"));
    }

    // --- I1: --dry-run must show -env:UserInstallation for every soffice
    // step, not silently omit it -------------------------------------------

    /// A single-step Soffice recipe (`docx -> pdf`) must carry the
    /// placeholder as its first argv element — the exact position
    /// `exec::run` substitutes into.
    #[test]
    fn soffice_step_shows_the_user_installation_placeholder_first_in_its_argv() {
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Soffice);
        assert_eq!(
            plan.steps[0].argv.first().map(String::as_str),
            Some(USER_INSTALLATION_PLACEHOLDER)
        );
        // The rest of the recipe's own argv follows, unperturbed.
        assert!(plan.steps[0].argv.contains(&"--headless".to_string()));
    }

    /// A two-step recipe (`md -> pdf`: pandoc then soffice) must only
    /// prepend the placeholder onto the *soffice* step, never the pandoc
    /// one — this is a per-backend behaviour, not a per-plan one.
    #[test]
    fn only_the_soffice_step_of_a_multi_step_plan_gets_the_placeholder() {
        let plan = build(
            Format::Md,
            Format::Pdf,
            &[p("a.md")],
            Path::new("out/b.pdf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].backend, Backend::Pandoc);
        assert!(
            !plan.steps[0]
                .argv
                .contains(&USER_INSTALLATION_PLACEHOLDER.to_string()),
            "{:?}",
            plan.steps[0].argv
        );
        assert_eq!(plan.steps[1].backend, Backend::Soffice);
        assert_eq!(
            plan.steps[1].argv.first().map(String::as_str),
            Some(USER_INSTALLATION_PLACEHOLDER)
        );
    }

    // --- Availability-based selection for docx/odt -> pdf ------------------

    fn avail(backends: &[Backend]) -> AvailableBackends {
        backends.iter().copied().collect()
    }

    /// `None` — no availability hint at all — must always yield the
    /// canonical (soffice) recipe. This is what keeps the argv snapshot
    /// (which calls `build` with `None` for every pair) byte-identical to
    /// before this selection existed.
    #[test]
    fn no_availability_hint_yields_the_canonical_soffice_recipe() {
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Soffice);
    }

    /// Both routes available: soffice wins.
    #[test]
    fn selection_prefers_soffice_when_both_routes_are_available() {
        let available = avail(&[Backend::Soffice, Backend::Pandoc, Backend::Typst]);
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            Some(&available),
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Soffice);
    }

    /// Only soffice available: soffice, obviously — there is no other route.
    #[test]
    fn selection_uses_soffice_when_only_soffice_is_available() {
        let available = avail(&[Backend::Soffice]);
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            Some(&available),
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Soffice);
    }

    /// Only pandoc+typst available (soffice absent): the fallback recipe is
    /// chosen, and its argv carries the `--pdf-engine` flag and a
    /// placeholder for the resolved typst path.
    #[test]
    fn selection_falls_back_to_pandoc_and_typst_when_soffice_is_unavailable() {
        let available = avail(&[Backend::Pandoc, Backend::Typst]);
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            Some(&available),
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Pandoc);
        assert!(
            plan.steps[0].argv.contains(&"--pdf-engine".to_string()),
            "{:?}",
            plan.steps[0].argv
        );
        assert!(
            plan.steps[0].argv.iter().any(|a| a.contains("typst")),
            "{:?}",
            plan.steps[0].argv
        );
    }

    /// Neither route fully available (soffice absent, and only one of
    /// pandoc/typst present, or neither): must still fall back to the
    /// canonical soffice recipe, so the user gets the ordinary
    /// `backend_missing` naming soffice — the pair's own primary backend —
    /// rather than a confusing one naming typst.
    #[test]
    fn selection_falls_back_to_soffice_when_neither_route_is_fully_available() {
        for available in [
            avail(&[]),
            avail(&[Backend::Pandoc]),
            avail(&[Backend::Typst]),
        ] {
            let plan = build(
                Format::Docx,
                Format::Pdf,
                &[p("in.docx")],
                Path::new("out.pdf"),
                None,
                Some(&available),
            )
            .unwrap();
            assert_eq!(plan.steps[0].backend, Backend::Soffice);
        }
    }

    /// `odt -> pdf` gets the same fallback treatment as `docx -> pdf`.
    #[test]
    fn odt_to_pdf_also_falls_back_to_pandoc_and_typst() {
        let available = avail(&[Backend::Pandoc, Backend::Typst]);
        let plan = build(
            Format::Odt,
            Format::Pdf,
            &[p("in.odt")],
            Path::new("out.pdf"),
            None,
            Some(&available),
        )
        .unwrap();
        assert_eq!(plan.steps[0].backend, Backend::Pandoc);
    }

    /// pandoc cannot read spreadsheets or slide decks, so `xlsx`/`pptx` ->
    /// `pdf` must stay LibreOffice-only even when soffice is unavailable and
    /// pandoc+typst both are.
    #[test]
    fn xlsx_and_pptx_never_fall_back_even_when_soffice_is_unavailable() {
        let available = avail(&[Backend::Pandoc, Backend::Typst]);
        for from in [Format::Xlsx, Format::Pptx] {
            let input = PathBuf::from(format!("in.{}", from.ext()));
            let plan = build(
                from,
                Format::Pdf,
                &[input],
                Path::new("out.pdf"),
                None,
                Some(&available),
            )
            .unwrap();
            assert_eq!(plan.steps[0].backend, Backend::Soffice, "{from:?}");
        }
    }

    /// The fallback recipe must carry its fidelity warning on the plan.
    #[test]
    fn fallback_recipe_carries_its_fidelity_warning() {
        let available = avail(&[Backend::Pandoc, Backend::Typst]);
        let plan = build(
            Format::Docx,
            Format::Pdf,
            &[p("in.docx")],
            Path::new("out.pdf"),
            None,
            Some(&available),
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].contains("LibreOffice"),
            "{:?}",
            plan.warnings
        );
    }

    // --- Tuning: slots fill through the full build path, and a flag with
    // no slot is refused, never silently ignored -------------------------

    fn tuned(resize: Option<&str>, quality: Option<u8>, colors: Option<u16>) -> Tuning {
        Tuning {
            resize: resize.map(str::to_owned),
            quality,
            colors,
            fps: None,
            crf: None,
            max_size: None,
            upscale: false,
            strip_metadata: false,
            range: None,
            mute: false,
            speed: None,
            shift_pitch: false,
        }
    }

    #[test]
    fn build_tuned_fills_the_image_recipe_slots() {
        let plan = build_tuned(
            Format::Heic,
            Format::Jpg,
            &[p("in.heic")],
            Path::new("out.jpg"),
            None,
            None,
            &tuned(Some("1600x900"), Some(70), Some(64)),
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-resize", "1600x900>"]),
            "{argv:?}"
        );
        assert!(argv.windows(2).any(|w| w == ["-colors", "64"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-quality", "70"]), "{argv:?}");
        assert!(!argv.contains(&"92".to_string()), "{argv:?}");
    }

    #[test]
    fn build_without_tuning_is_byte_identical_to_the_static_table() {
        let untuned = build(
            Format::Heic,
            Format::Jpg,
            &[p("in.heic")],
            Path::new("out.jpg"),
            None,
            None,
        )
        .unwrap();
        let default_tuned = build_tuned(
            Format::Heic,
            Format::Jpg,
            &[p("in.heic")],
            Path::new("out.jpg"),
            None,
            None,
            &Tuning::default(),
        )
        .unwrap();
        assert_eq!(untuned, default_tuned);
        assert!(
            untuned.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-quality", "92"]),
            "{:?}",
            untuned.steps[0].argv
        );
    }

    /// `--quality` on a lossless target: refused with the honest reason,
    /// while `--resize` alone on the same pair still works.
    #[test]
    fn quality_on_a_lossless_target_is_refused_but_resize_works() {
        let e = build_tuned(
            Format::Heic,
            Format::Png,
            &[p("in.heic")],
            Path::new("out.png"),
            None,
            None,
            &tuned(None, Some(70), None),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert!(e.message.contains("lossless"), "{}", e.message);

        let plan = build_tuned(
            Format::Heic,
            Format::Png,
            &[p("in.heic")],
            Path::new("out.png"),
            None,
            None,
            &tuned(Some("50%"), None, None),
        )
        .unwrap();
        assert!(
            plan.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-resize", "50%>"]),
            "{:?}",
            plan.steps[0].argv
        );
    }

    /// Tuning on non-image pairs is refused on both the static-transcode
    /// path and the probe-selected stream-copy path.
    #[test]
    fn tuning_on_media_pairs_is_refused_on_both_paths() {
        let e = build_tuned(
            Format::Mp4,
            Format::Mp3,
            &[p("in.mp4")],
            Path::new("out.mp3"),
            None,
            None,
            &tuned(Some("50%"), None, None),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert!(e.message.contains("--resize"), "{}", e.message);

        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let e = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &tuned(None, Some(50), None),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert!(e.message.contains("--quality"), "{}", e.message);
    }

    // --- The dispatch consults the tuning before choosing the path, so a
    // video knob on a media pair transcodes instead of always being
    // refused --------------------------------------------------------------

    // These pass `None` for `available`, as every dynamic-media test above
    // does: it only matters to `select`, never to the probe-aware branch
    // these tests exercise.

    #[test]
    fn a_video_knob_on_a_remuxable_pair_transcodes_instead_of_refusing() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let t = Tuning {
            fps: Some("24".into()),
            ..Default::default()
        };
        let plan = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &t,
        )
        .expect("a video knob must not refuse a pair the user expects to work");
        let argv = &plan.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "{argv:?}"
        );
        assert!(argv.iter().any(|a| a.contains("fps=24")), "{argv:?}");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Re-encoded rather than stream-copied")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn an_image_knob_on_a_media_pair_is_still_refused_by_its_own_name() {
        // The old validator's final else said "--colors" unconditionally,
        // so a new field would have reported itself as --colors.
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            ..MediaProbe::default()
        };
        let t = Tuning {
            quality: Some(80),
            ..Default::default()
        };
        let err = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &t,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--quality"), "{err}");
        assert!(!err.to_string().contains("--colors"), "{err}");
    }

    #[test]
    fn no_refusal_message_says_for_now_any_more() {
        // "(for now)" became false the day --fps shipped, in the very
        // message a user sees when a video knob is refused.
        let t = Tuning {
            fps: Some("24".into()),
            ..Default::default()
        };
        let err = build_tuned(
            Format::Png,
            Format::Jpg,
            &[p("in.png")],
            Path::new("out.jpg"),
            None,
            None,
            &t,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--fps"), "{err}");
        assert!(!err.to_string().contains("for now"), "{err}");
    }

    #[test]
    fn an_untuned_plan_is_byte_identical_to_the_untuned_builder() {
        // This is the assertion in `build_without_tuning_is_byte_identical_
        // to_the_static_table` above, re-run against the probe-aware
        // dynamic-media branch this task inverts; it must keep holding.
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let a = build(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
        );
        let b = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &Tuning::default(),
        );
        assert_eq!(a.unwrap().steps[0].argv, b.unwrap().steps[0].argv);
    }

    #[test]
    fn a_cap_that_does_not_bind_reaches_the_plans_warnings() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some((24, 1)),
            ..MediaProbe::default()
        };
        let t = Tuning {
            fps: Some("30".into()),
            ..Default::default()
        };
        let plan = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &t,
        )
        .unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w == "Source is 24 fps; --fps 30 left it unchanged."),
            "{:?}",
            plan.warnings
        );
        // A cap that does not bind must take the stream-copy path, not pay
        // for a full re-encode that changes nothing but the file's size:
        // asserting only that the note is present passed under both the
        // buggy `tuning.fps.is_some()` predicate and the fixed
        // `resolved.fps.is_some()` one, so it could not catch a regression
        // back to the former.
        assert!(
            !plan.steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["-c:v", "libx264"]),
            "a non-binding cap must not force a re-encode: {:?}",
            plan.steps[0].argv
        );
        assert!(
            !plan
                .warnings
                .iter()
                .any(|w| w.contains("Re-encoded rather than stream-copied")),
            "a non-binding cap must not claim a re-encode that never happened: {:?}",
            plan.warnings
        );
    }

    /// The `--resize` twin of the test above: a size the source already
    /// fits within keeps the stream copy, as `--fps` does.
    #[test]
    fn a_resize_that_does_not_bind_keeps_the_stream_copy() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &Tuning {
                resize: Some("4000x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            !argv.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "a non-binding cap must not force a re-encode: {argv:?}"
        );
        assert!(!argv.iter().any(|a| a.contains("scale=")), "{argv:?}");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w == "Source is 1280x720; --resize 4000x left it unchanged (add --upscale to enlarge)."),
            "{:?}",
            plan.warnings
        );
        assert!(
            !plan
                .warnings
                .iter()
                .any(|w| w.contains("Re-encoded rather than stream-copied")),
            "{:?}",
            plan.warnings
        );
    }

    fn upscaled(geometry: &str) -> Tuning {
        Tuning {
            resize: Some(geometry.into()),
            upscale: true,
            ..Default::default()
        }
    }

    /// An image's size is not read before converting, so `--upscale` on
    /// one always carries the warning, worded for a size it cannot know.
    #[test]
    fn upscale_lets_an_image_resize_enlarge_and_warns() {
        let plan = build_tuned(
            Format::Png,
            Format::Jpg,
            &[p("in.png")],
            Path::new("out.jpg"),
            None,
            None,
            &upscaled("1600x900"),
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-resize", "1600x900"]),
            "{argv:?}"
        );
        assert_eq!(
            plan.enlarged.as_ref().map(|e| e.warning.as_str()),
            Some(
                "--resize 1600x900 --upscale enlarges any source smaller than that, and this \
                 one's size is not known: enlarging adds no detail, so expect a soft picture \
                 and a much larger file."
            )
        );
    }

    #[test]
    fn upscale_lets_a_gif_resize_enlarge_and_warns() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            Some(&probe),
            None,
            &upscaled("4000x"),
        )
        .unwrap();
        let vf = plan.steps[0].argv.join(" ");
        assert!(vf.contains("scale=w=4000:h=-2:flags=lanczos"), "{vf}");
        let e = plan.enlarged.expect("an enlarged GIF must warn");
        assert!(
            e.warning
                .starts_with("--resize 4000x --upscale enlarges the 1280x720 source to 4000x2250"),
            "{}",
            e.warning
        );
        assert!(e.needs_confirmation, "9.8 times the pixels asks first");
        assert!(plan.warnings.iter().all(|w| !w.contains("left it")));
    }

    #[test]
    fn upscale_lets_a_video_resize_enlarge_and_warns() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            width: Some(640),
            height: Some(360),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &upscaled("1280x"),
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(
            argv.iter().any(|a| a.contains("scale=w=1280:h=-2")),
            "{argv:?}"
        );
        assert_eq!(
            plan.enlarged.as_ref().map(|e| e.warning.as_str()),
            Some(
                "--resize 1280x --upscale enlarges the 640x360 source to 1280x720, about 4 \
                 times its pixels: enlarging adds no detail, so expect a soft picture and a \
                 much larger file."
            )
        );
    }

    #[test]
    fn upscale_is_named_when_a_pair_takes_no_resize() {
        let e = build_tuned(
            Format::Mp4,
            Format::Mp3,
            &[p("in.mp4")],
            Path::new("out.mp3"),
            None,
            None,
            &upscaled("50%"),
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "--resize and --upscale do not apply to mp4 -> mp3: they tune image, video and GIF \
             targets"
        );
    }

    /// On GIF, a `--resize` the source already fits within keeps the
    /// source's size, as it did when the clamp did that inside the filter:
    /// it still replaces the recipe's 640 default.
    #[test]
    fn a_gif_resize_past_the_source_keeps_the_source_size() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            Some(&probe),
            None,
            &Tuning {
                resize: Some("4000x".into()),
                ..Default::default()
            },
        )
        .expect("mp4 -> gif is a registered pair");
        let vf = plan.steps[0].argv.join(" ");
        assert!(
            !vf.contains("min(640"),
            "the 640 default must not return: {vf}"
        );
        assert!(vf.contains("split[a][b]"), "{vf}");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w == "Source is 1280x720; --resize 4000x left it unchanged, wider than the GIF default of 640 (add --upscale to enlarge)."),
            "{:?}",
            plan.warnings
        );
    }

    /// The 0-51 `--crf` bound in `validate_tuning` once ran only on the
    /// *static* table. Once a video
    /// knob routes a probe-selected pair through `transcoded_invocation`
    /// instead, that check never ran and `--crf 60` on an mp4 target sailed
    /// through unrefused. `check_crf_range` is now shared by both
    /// validators so the bound holds on whichever path actually fires.
    #[test]
    fn crf_out_of_range_is_refused_on_the_probe_selected_path_too() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            ..MediaProbe::default()
        };
        let t = Tuning {
            crf: Some(60),
            ..Default::default()
        };
        let err = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &t,
        )
        .unwrap_err();
        assert_eq!(err.code, crate::ErrorCode::InvalidInvocation);
        assert!(err.message.contains("--crf 60"), "{}", err.message);
        assert!(err.message.contains("out of range"), "{}", err.message);
    }

    // --- A video knob must reach a static recipe's chain too, not only the
    // probe-selected dynamic path ----------------------------------------

    #[test]
    fn a_video_knob_reaches_a_static_recipes_chain() {
        // mp4 -> gif never takes the dynamic path: compat_tables(Gif) is
        // None, so transcoded_invocation declines and control falls through
        // to TO_GIF. The knob must still land in the chain.
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let t = Tuning {
            fps: Some("10".into()),
            resize: Some("320x".into()),
            ..Default::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            Some(&probe),
            None,
            &t,
        )
        .expect("mp4 -> gif is a registered pair");
        let vf = plan.steps[0].argv.join(" ");
        assert!(
            vf.contains("fps=10"),
            "the authored fps=15 must be overridden: {vf}"
        );
        assert!(!vf.contains("fps=15"), "{vf}");
        assert!(
            vf.contains("min(320"),
            "the authored width must be overridden: {vf}"
        );
        assert!(!vf.contains("min(640"), "{vf}");
        // The palette chain must survive intact -- spliced after split[a][b]
        // the tuned values would produce a default web-palette GIF.
        assert!(
            vf.find("fps=10").unwrap() < vf.find("split[a][b]").unwrap(),
            "{vf}"
        );
    }

    /// `--fps` replaces GIF's authored 15 whether or not it binds against
    /// the source. When it does not bind, the output keeps the source's own
    /// rate, as the note says -- not the recipe's 15, which made `--fps 30`
    /// on a 30 fps source give half the frames `--fps 29` did.
    #[test]
    fn a_gif_fps_at_or_above_the_source_keeps_the_source_rate() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        for fps in ["30", "60"] {
            let plan = build_tuned(
                Format::Mp4,
                Format::Gif,
                &[p("in.mp4")],
                Path::new("out.gif"),
                Some(&probe),
                None,
                &Tuning {
                    fps: Some(fps.into()),
                    ..Default::default()
                },
            )
            .expect("mp4 -> gif is a registered pair");
            let vf = plan.steps[0].argv.join(" ");
            assert!(
                !vf.contains("fps="),
                "--fps {fps} must lift the authored cap, not keep it: {vf}"
            );
            let note = format!("Source is 30 fps; --fps {fps} left it unchanged.");
            assert!(plan.warnings.contains(&note), "{:?}", plan.warnings);
        }
    }

    #[test]
    fn an_untuned_static_recipe_is_unchanged_by_the_hoist() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            ..MediaProbe::default()
        };
        let a = build(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            Some(&probe),
            None,
        );
        let b = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            Some(&probe),
            None,
            &Tuning::default(),
        );
        assert_eq!(a.unwrap().steps[0].argv, b.unwrap().steps[0].argv);
    }

    /// The other half of the bug: `gif -> mp4` never even reaches the
    /// probe branch (`registry::needs_probe(Gif, Mp4)` is `false`), so the
    /// hoist has to serve the static path unconditionally, not just when
    /// the probe branch happens to run.
    #[test]
    fn a_video_knob_reaches_the_gif_to_mp4_static_recipe() {
        let probe = MediaProbe {
            video_codec: Some("gif".into()),
            video_streams: 1,
            width: Some(480),
            height: Some(270),
            frame_rate: Some((15, 1)),
            ..MediaProbe::default()
        };
        let t = Tuning {
            fps: Some("10".into()),
            resize: Some("160x".into()),
            ..Default::default()
        };
        let plan = build_tuned(
            Format::Gif,
            Format::Mp4,
            &[p("in.gif")],
            Path::new("out.mp4"),
            Some(&probe),
            None,
            &t,
        )
        .expect("gif -> mp4 is a registered pair");
        let vf = plan.steps[0].argv.join(" ");
        assert!(vf.contains("fps=10"), "{vf}");
        assert!(vf.contains("min(160"), "{vf}");
    }

    /// A cap that cannot be resolved (no probe at all) must still leave a
    /// note on the plan, now that `resolved` reaches the static path even
    /// with `probe: None` -- this is the mechanism that lets a real run
    /// with a failed probe say so too, not only `--dry-run`.
    #[test]
    fn an_unresolvable_cap_on_a_static_recipe_notes_it_was_applied_as_given() {
        let t = Tuning {
            fps: Some("24".into()),
            ..Default::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Gif,
            &[p("in.mp4")],
            Path::new("out.gif"),
            None,
            None,
            &t,
        )
        .unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains(
                "Source frame rate could not be determined; --fps 24 was applied as given."
            )),
            "{:?}",
            plan.warnings
        );
        assert!(plan.steps[0].argv.iter().any(|a| a.contains("fps=24")));
    }

    #[test]
    fn a_webm_video_knob_without_a_probe_names_the_real_cause() {
        let e = build_tuned(
            Format::Mp4,
            Format::Webm,
            &[p("in.mp4")],
            Path::new("out.webm"),
            None,
            None,
            &Tuning {
                fps: Some("15".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.message.contains("needs ffprobe"), "{}", e.message);
        assert!(
            !e.message.contains("it tunes video and GIF targets"),
            "webm is a video target: {}",
            e.message
        );
    }

    /// The other side of the guard: a cap that does not bind (a 60 fps cap
    /// on a 24 fps source) resolves to nothing, so nothing needs a probe to
    /// apply it. h264 is not in the webm table, so the stream copy is not
    /// available either and this lands on the static webm recipe -- which
    /// must run, as a no-op for the knob, not claim ffprobe failed when it
    /// just answered.
    #[test]
    fn a_non_binding_fps_on_a_probed_webm_target_is_a_noted_no_op() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((24, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Webm,
            &[p("in.mp4")],
            Path::new("out.webm"),
            Some(&probe),
            None,
            &Tuning {
                fps: Some("60".into()),
                ..Default::default()
            },
        )
        .expect("a cap that does not bind is not a refusal");
        let argv = &plan.steps[0].argv;
        assert!(argv.iter().any(|a| a == "libvpx-vp9"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "-vf"), "{argv:?}");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("Source is 24 fps; --fps 60 left it unchanged.")),
            "{:?}",
            plan.warnings
        );
    }

    /// The `--resize` twin of the test above: a size the source already
    /// fits within resolves to no filter, so the static webm recipe runs
    /// rather than `--resize` being refused as a flag webm cannot take.
    #[test]
    fn a_non_binding_resize_on_a_probed_webm_target_is_a_noted_no_op() {
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((24, 1)),
            ..MediaProbe::default()
        };
        let plan = build_tuned(
            Format::Mp4,
            Format::Webm,
            &[p("in.mp4")],
            Path::new("out.webm"),
            Some(&probe),
            None,
            &Tuning {
                resize: Some("4000x".into()),
                ..Default::default()
            },
        )
        .expect("a cap that does not bind is not a refusal");
        let argv = &plan.steps[0].argv;
        assert!(argv.iter().any(|a| a == "libvpx-vp9"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "-vf"), "{argv:?}");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w == "Source is 1280x720; --resize 4000x left it unchanged (add --upscale to enlarge)."),
            "{:?}",
            plan.warnings
        );
    }

    /// A probe that read no video stream leaves `--fps` applied as given
    /// (there is no source rate to cap against), which the static webm
    /// recipe cannot do -- so this is still the "needs ffprobe" refusal, not
    /// a silent no-op.
    #[test]
    fn a_webm_fps_on_a_probe_that_read_no_video_still_needs_ffprobe() {
        let e = build_tuned(
            Format::Mp4,
            Format::Webm,
            &[p("in.mp4")],
            Path::new("out.webm"),
            Some(&MediaProbe::default()),
            None,
            &Tuning {
                fps: Some("15".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            e.message.starts_with("--fps on mp4 -> webm needs ffprobe"),
            "{}",
            e.message
        );
    }

    /// `--resize` always resolves to a scale filter, so on a probe that read
    /// no video stream (nothing for `transcoded_invocation` to map) it is
    /// refused as the probe's failure, naming `--resize`.
    #[test]
    fn a_webm_resize_on_a_probe_that_read_no_video_names_resize() {
        let e = build_tuned(
            Format::Mp4,
            Format::Webm,
            &[p("in.mp4")],
            Path::new("out.webm"),
            Some(&MediaProbe::default()),
            None,
            &Tuning {
                resize: Some("640x".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            e.message
                .starts_with("--resize on mp4 -> webm needs ffprobe"),
            "{}",
            e.message
        );
    }

    /// The `--json` plan envelope is a published contract: an unsized plan
    /// must not grow a `sizing` key.
    #[test]
    fn an_unsized_plan_serialises_without_a_sizing_key() {
        let plan = build(
            Format::Png,
            Format::Jpg,
            &[p("in.png")],
            Path::new("out.jpg"),
            None,
            None,
        )
        .unwrap();
        let v = serde_json::to_value(&plan).unwrap();
        assert!(v.get("sizing").is_none(), "{v}");
    }

    // --- Ranges: --start / --end / --duration ------------------------------

    fn ranged(start: Option<&str>, end: Option<&str>) -> Tuning {
        Tuning {
            range: crate::trim::Range::new(
                start.map(|s| crate::trim::parse_time(s).unwrap()),
                end.map(|s| crate::trim::parse_time(s).unwrap()),
                None,
            )
            .unwrap(),
            ..Tuning::default()
        }
    }

    fn clip_probe(secs: u64) -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            video_streams: 1,
            audio_codecs: vec!["aac".into()],
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((30, 1)),
            duration_ms: Some(secs * 1000),
            size_bytes: Some(secs * 250_000),
            ..MediaProbe::default()
        }
    }

    fn cut_plan(from: Format, to: Format, t: &Tuning, secs: u64) -> Result<ConversionPlan> {
        let input = p(&format!("in.{}", from.ext()));
        let output = format!("out.{}", to.ext());
        build_tuned(
            from,
            to,
            &[input],
            Path::new(&output),
            Some(&clip_probe(secs)),
            None,
            t,
        )
    }

    #[test]
    fn a_cut_from_zero_keeps_the_stream_copy() {
        let plan = cut_plan(Format::Mkv, Format::Mp4, &ranged(None, Some("30")), 600).unwrap();
        let argv = &plan.steps[0].argv;
        assert_eq!(&argv[..4], ["-t", "30", "-i", "in.mkv"], "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-c:v", "copy"]), "{argv:?}");
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.steps[0].path_args, [3, argv.len() - 1]);
        let r = plan.range.expect("a cut plan reports its range");
        assert_eq!((r.start_ms, r.end_ms), (0, Some(30_000)));
    }

    /// With B-frames, a frame shown before the end can be stored after a
    /// frame shown after it, so a copy that stops at the end carries two
    /// frames past it (measured on 6.1 and 9.0). A copy to the end of the
    /// file has no end to run past.
    #[test]
    fn a_cut_from_zero_of_video_with_b_frames_re_encodes_to_end_exactly() {
        let mut probe = clip_probe(600);
        probe.video_reorders = true;
        let plan = |t: &Tuning| {
            build_tuned(
                Format::Mkv,
                Format::Mp4,
                &[p("in.mkv")],
                Path::new("out.mp4"),
                Some(&probe),
                None,
                t,
            )
            .unwrap()
        };
        let cut = plan(&ranged(None, Some("30")));
        let argv = &cut.steps[0].argv;
        assert!(
            argv.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "{argv:?}"
        );
        assert_eq!(
            cut.warnings,
            [
                "Re-encoded rather than stream-copied, because this video stores some frames \
                 after ones shown later, so a copied cut would run past its end; this one \
                 ends exactly at 0:30."
            ]
        );
        let to_the_end = plan(&ranged(Some("0"), Some("20:00")));
        let argv = &to_the_end.steps[0].argv;
        assert!(argv.windows(2).any(|w| w == ["-c:v", "copy"]), "{argv:?}");
    }

    /// A clip is not a copy of the whole file, so a cut into its own format
    /// that also strips takes the cut's path and clears the tags there,
    /// copying or re-encoding as any cut would.
    #[test]
    fn a_cut_into_its_own_format_strips_as_it_cuts() {
        for (start, copied) in [(None, true), (Some("1:02"), false)] {
            let mut t = ranged(start, Some("1:10"));
            t.strip_metadata = true;
            let plan = cut_plan(Format::Mp4, Format::Mp4, &t, 600).unwrap();
            let argv = &plan.steps[0].argv;
            let has = |pair: [&str; 2]| argv.windows(2).any(|w| w == pair);
            assert!(has(["-t", "8"]) || has(["-t", "70"]), "{argv:?}");
            assert!(has(["-map_metadata", "-1"]), "{argv:?}");
            assert_eq!(has(["-c:v", "copy"]), copied, "{argv:?}");
            assert!(plan.range.is_some());
        }
    }

    #[test]
    fn a_cut_after_zero_re_encodes_to_start_exactly_and_says_why() {
        let plan = cut_plan(
            Format::Mkv,
            Format::Mp4,
            &ranged(Some("1:02"), Some("1:10")),
            600,
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert_eq!(
            &argv[..6],
            ["-ss", "62", "-t", "8", "-i", "in.mkv"],
            "{argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w == ["-c:v", "libx264"]),
            "{argv:?}"
        );
        assert_eq!(
            plan.warnings,
            [
                "Re-encoded rather than stream-copied, because a copied cut can only start \
              on a keyframe; this one starts exactly at 1:02."
            ]
        );
        assert_eq!(plan.steps[0].path_args, [5, argv.len() - 1]);
    }

    #[test]
    fn a_knob_with_a_cut_says_the_knob_forced_the_encode() {
        let mut t = ranged(Some("1:02"), Some("1:10"));
        t.fps = Some("24".into());
        let plan = cut_plan(Format::Mkv, Format::Mp4, &t, 600).unwrap();
        assert!(plan.steps[0].argv.iter().any(|a| a.contains("fps=24")));
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(plan.warnings[0].contains("a video knob changes the picture"));
    }

    #[test]
    fn an_audio_copy_keeps_the_copy_whatever_the_start() {
        let plan = cut_plan(
            Format::Mp4,
            Format::M4a,
            &ranged(Some("1:02"), Some("1:10")),
            600,
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert_eq!(
            &argv[..6],
            ["-ss", "62", "-t", "8", "-i", "in.mp4"],
            "{argv:?}"
        );
        assert!(argv.windows(2).any(|w| w == ["-c:a", "copy"]), "{argv:?}");
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_static_recipe_takes_the_cut_in_its_slot() {
        let plan = cut_plan(
            Format::Mp4,
            Format::Mp3,
            &ranged(Some("1:02"), Some("1:10")),
            600,
        )
        .unwrap();
        assert_eq!(
            plan.steps[0].argv,
            [
                "-ss",
                "62",
                "-t",
                "8",
                "-i",
                "in.mp4",
                "-vn",
                "-c:a",
                "libmp3lame",
                "-q:a",
                "2",
                "-y",
                "-map_chapters",
                "-1",
                "-copypriorss:s",
                "0",
                "out.mp3"
            ]
        );
    }

    #[test]
    fn the_gif_memory_note_is_about_the_clip_not_the_file() {
        let short = cut_plan(
            Format::Mp4,
            Format::Gif,
            &ranged(Some("1:00"), Some("1:10")),
            600,
        )
        .unwrap();
        assert!(
            short
                .warnings
                .iter()
                .all(|w| !w.contains("buffered in memory")),
            "{:?}",
            short.warnings
        );
        let long = cut_plan(
            Format::Mp4,
            Format::Gif,
            &ranged(Some("1:00"), Some("2:00")),
            600,
        )
        .unwrap();
        assert!(
            long.warnings
                .iter()
                .any(|w| w.contains("buffered in memory")),
            "{:?}",
            long.warnings
        );
    }

    #[test]
    fn a_range_on_a_pair_without_a_timeline_is_refused_by_name() {
        for (from, to, why) in [
            (Format::Png, Format::Jpg, "jpg is a still image"),
            (Format::Gif, Format::Png, "png is a still image"),
            (
                Format::Docx,
                Format::Pdf,
                "docx -> pdf has no timeline to cut",
            ),
        ] {
            let input = p(&format!("in.{}", from.ext()));
            let e = build_tuned(
                from,
                to,
                &[input],
                Path::new("out"),
                None,
                None,
                &ranged(Some("5"), None),
            )
            .unwrap_err();
            assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
            assert_eq!(
                e.message,
                format!(
                    "--start does not apply to {} -> {}: {why}; --start cuts video and audio",
                    from.ext(),
                    to.ext()
                )
            );
        }
    }

    #[test]
    fn a_range_without_a_probe_names_ffprobe() {
        let e = build_tuned(
            Format::Mkv,
            Format::Mp4,
            &[p("in.mkv")],
            Path::new("o.mp4"),
            None,
            None,
            &ranged(Some("5"), None),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::ConversionFailed);
        assert!(e.message.contains("needs ffprobe"), "{}", e.message);
    }

    #[test]
    fn a_whole_file_range_converts_as_if_none_were_given_with_a_note() {
        let plan = cut_plan(Format::Mkv, Format::Mp4, &ranged(None, Some("20:00")), 600).unwrap();
        assert_eq!(plan.steps[0].argv[0], "-i");
        assert!(plan.steps[0].argv.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert_eq!(
            plan.warnings,
            ["The range covers all of the 10:00 source, so nothing was cut."]
        );
    }

    #[test]
    fn a_plan_without_a_range_serialises_without_one() {
        let plan = build(
            Format::Mp4,
            Format::Gif,
            &[p("a.mp4")],
            Path::new("a.gif"),
            None,
            None,
        )
        .unwrap();
        assert!(serde_json::to_value(&plan).unwrap().get("range").is_none());
    }

    #[test]
    fn a_video_cut_into_its_own_format_copies_from_zero_and_encodes_after() {
        let from_zero = cut_plan(Format::Mp4, Format::Mp4, &ranged(None, Some("30")), 600).unwrap();
        assert!(from_zero.steps[0]
            .argv
            .windows(2)
            .any(|w| w == ["-c:v", "copy"]));
        let later = cut_plan(Format::Mp4, Format::Mp4, &ranged(Some("1:02"), None), 600).unwrap();
        assert!(later.steps[0]
            .argv
            .windows(2)
            .any(|w| w == ["-c:v", "libx264"]));
        assert_eq!(&later.steps[0].argv[..3], ["-ss", "62", "-i"]);
    }

    #[test]
    fn an_audio_file_cut_into_its_own_format_copies_what_it_can() {
        let mut probe = clip_probe(600);
        probe.video_codec = None;
        probe.video_streams = 0;
        probe.audio_codecs = vec!["aac".into()];
        let plan = build_tuned(
            Format::M4a,
            Format::M4a,
            &[p("memo.m4a")],
            Path::new("o.m4a"),
            Some(&probe),
            None,
            &ranged(Some("10"), Some("20")),
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.windows(2).any(|w| w == ["-c:a", "copy"]),
            "{:?}",
            plan.steps[0].argv
        );

        // A file cut into its own format keeps its own codec: a 24-bit or
        // float take stays what it was, though converting to wav from
        // anything else writes 16-bit. PCM is written out again in that
        // codec rather than copied, which is as lossless and cuts to the
        // sample.
        for codec in ["pcm_s24le", "pcm_f32le"] {
            probe.audio_codecs = vec![codec.into()];
            let plan = build_tuned(
                Format::Wav,
                Format::Wav,
                &[p("take.wav")],
                Path::new("o.wav"),
                Some(&probe),
                None,
                &ranged(Some("10"), Some("20")),
            )
            .unwrap();
            let argv = &plan.steps[0].argv;
            assert_eq!(&argv[..6], ["-ss", "10", "-t", "10", "-i", "take.wav"]);
            assert!(argv.windows(2).any(|w| w == ["-c:a", codec]), "{argv:?}");
            assert!(plan.warnings.is_empty(), "{codec}: {:?}", plan.warnings);
        }
    }

    /// A copied FLAC cut keeps the source's header, which gives the whole
    /// source's length, and stops on a frame boundary. Written out again
    /// as FLAC it is as lossless, says its own length and is cut to the
    /// sample.
    #[test]
    fn a_flac_cut_into_flac_is_written_out_again() {
        let mut probe = clip_probe(600);
        probe.video_codec = None;
        probe.video_streams = 0;
        probe.audio_codecs = vec!["flac".into()];
        let plan = build_tuned(
            Format::Flac,
            Format::Flac,
            &[p("song.flac")],
            Path::new("o.flac"),
            Some(&probe),
            None,
            &ranged(Some("10"), Some("20")),
        )
        .unwrap();
        let argv = &plan.steps[0].argv;
        assert!(argv.windows(2).any(|w| w == ["-c:a", "flac"]), "{argv:?}");
        assert!(!argv.windows(2).any(|w| w == ["-c:a", "copy"]), "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-c:v", "copy"]), "cover art");
    }

    #[test]
    fn a_format_into_itself_without_a_range_is_still_no_conversion() {
        let e = build_tuned(
            Format::Mp4,
            Format::Mp4,
            &[p("a.mp4")],
            Path::new("b.mp4"),
            Some(&clip_probe(60)),
            None,
            &Tuning::default(),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::UnsupportedPair);
    }

    // --- A silent clip (conv trim's video-only clips) -----------------------

    fn muted(start: Option<&str>, end: Option<&str>) -> Tuning {
        Tuning {
            mute: true,
            ..ranged(start, end)
        }
    }

    fn has(argv: &[String], pair: [&str; 2]) -> bool {
        argv.windows(2).any(|w| w == pair)
    }

    #[test]
    fn a_silent_clip_maps_no_audio() {
        let plan = cut_plan(Format::Mkv, Format::Mp4, &muted(None, Some("30")), 600).unwrap();
        let argv = &plan.steps[0].argv;
        assert!(argv.contains(&"-an".to_string()), "{argv:?}");
        assert!(!has(argv, ["-map", "0:a?"]), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "-copypriorss:a"), "{argv:?}");
        assert!(
            has(argv, ["-c:v", "copy"]),
            "a cut from 0 still copies: {argv:?}"
        );

        let plan = cut_plan(Format::Mkv, Format::Mkv, &muted(Some("5"), None), 600).unwrap();
        let argv = &plan.steps[0].argv;
        assert!(has(argv, ["-map", "0"]), "{argv:?}");
        assert!(argv.contains(&"-an".to_string()), "{argv:?}");
    }

    #[test]
    fn a_silent_clip_from_a_static_recipe_takes_its_slot() {
        let plan = build_tuned(
            Format::Avi,
            Format::Mp4,
            &[p("in.avi")],
            Path::new("out.mp4"),
            None,
            None,
            &Tuning {
                mute: true,
                ..Tuning::default()
            },
        )
        .unwrap();
        assert!(
            plan.steps[0].argv.contains(&"-an".to_string()),
            "{:?}",
            plan.steps[0].argv
        );
    }

    #[test]
    fn a_silent_clip_needs_a_video_target() {
        let e = cut_plan(Format::Mp4, Format::Mp3, &muted(Some("5"), None), 600).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "a silent clip needs a video target; mp3 has no picture"
        );
    }

    #[test]
    fn a_silent_clip_is_not_sized() {
        let mut t = muted(Some("5"), Some("10"));
        t.max_size = Some(crate::size::parse("5mb").unwrap());
        let e = cut_plan(Format::Mkv, Format::Mp4, &t, 600).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert!(e.message.contains("silent clip"), "{}", e.message);
    }

    // --- Speed: --speed and --shift-pitch -----------------------------------

    fn sped(speed: &str) -> Tuning {
        Tuning {
            speed: Some(crate::speed::parse(speed).unwrap()),
            ..Tuning::default()
        }
    }

    fn sped_plan(from: Format, to: Format, t: &Tuning, probe: &MediaProbe) -> ConversionPlan {
        let input = p(&format!("in.{}", from.ext()));
        let output = format!("out.{}", to.ext());
        build_tuned(from, to, &[input], Path::new(&output), Some(probe), None, t).unwrap()
    }

    fn value<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag);
        i.map(|i| argv[i + 1].as_str())
            .unwrap_or_else(|| panic!("no {flag} in {argv:?}"))
    }

    /// The output options a speed change adds, once, after the input.
    fn ends_retimed(argv: &[String]) {
        let input = argv.iter().position(|a| a == "-i").expect("-i");
        let at = argv
            .windows(3)
            .position(|w| w == ["-sn", "-map_chapters", "-1"])
            .unwrap_or_else(|| panic!("no -sn -map_chapters -1 in {argv:?}"));
        assert!(at > input, "{argv:?}");
        assert_eq!(
            argv.iter().filter(|a| *a == "-map_chapters").count(),
            1,
            "{argv:?}"
        );
    }

    #[test]
    fn a_video_sped_into_its_own_format_retimes_picture_and_sound() {
        let plan = sped_plan(Format::Mp4, Format::Mp4, &sped("2"), &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert!(value(argv, "-vf").starts_with("setpts=PTS/2,"), "{argv:?}");
        assert_eq!(value(argv, "-af"), "atempo=2");
        assert!(has(argv, ["-c:a", "aac"]), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "copy"), "{argv:?}");
        ends_retimed(argv);
        assert_eq!(
            serde_json::to_value(plan.speed.as_ref().unwrap()).unwrap(),
            serde_json::json!({"factor": 2.0, "shift_pitch": false})
        );
    }

    /// A speed change can never be a stream copy, so saying it was not one
    /// tells nobody anything; a knob alongside it still says so.
    #[test]
    fn a_speed_change_alone_does_not_explain_its_reencode() {
        let reencoded = |t: &Tuning| {
            sped_plan(Format::Mkv, Format::Mp4, t, &clip_probe(600))
                .warnings
                .iter()
                .any(|w| w.starts_with("Re-encoded rather than stream-copied"))
        };
        assert!(!reencoded(&sped("2")));
        let cut = Tuning {
            speed: Some(crate::speed::parse("2").unwrap()),
            ..ranged(Some("1:00"), Some("1:10"))
        };
        assert!(!reencoded(&cut));
        let resized = Tuning {
            resize: Some("640x".into()),
            ..sped("2")
        };
        assert!(reencoded(&resized));
    }

    #[test]
    fn a_container_change_that_would_copy_is_reencoded_for_a_speed_change() {
        let plan = sped_plan(Format::Mkv, Format::Mp4, &sped("1.5"), &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert!(has(argv, ["-c:v", "libx264"]), "{argv:?}");
        assert!(
            value(argv, "-vf").starts_with("setpts=PTS/1.5,"),
            "{argv:?}"
        );
        assert_eq!(value(argv, "-af"), "atempo=1.5");
    }

    #[test]
    fn a_speed_change_leaves_subtitles_unmapped_and_says_so() {
        let probe = MediaProbe {
            subtitle_codecs: vec!["subrip".into(), "mov_text".into()],
            ..clip_probe(600)
        };
        for to in [Format::Mp4, Format::Mkv] {
            let plan = sped_plan(Format::Mkv, to, &sped("2"), &probe);
            let argv = &plan.steps[0].argv;
            assert!(!argv.iter().any(|a| a.starts_with("0:s")), "{argv:?}");
            assert!(!argv.iter().any(|a| a == "-c:s"), "{argv:?}");
            ends_retimed(argv);
            assert!(
                plan.warnings
                    .iter()
                    .any(|w| w.starts_with("Subtitles are left out")),
                "{:?}",
                plan.warnings
            );
            assert!(
                !plan.warnings.iter().any(|w| w.contains("mov_text")),
                "{:?}",
                plan.warnings
            );
        }
    }

    #[test]
    fn sound_that_would_be_copied_is_filtered_for_a_speed_change() {
        let song = MediaProbe {
            audio_codecs: vec!["mp3".into()],
            audio_sample_rate: Some(44_100),
            duration_ms: Some(180_000),
            ..MediaProbe::default()
        };
        for (from, to, probe) in [
            (Format::Mp4, Format::M4a, clip_probe(600)),
            (Format::Mp3, Format::Mp3, song.clone()),
            (Format::Mp3, Format::Flac, song),
        ] {
            let plan = sped_plan(from, to, &sped("2"), &probe);
            let argv = &plan.steps[0].argv;
            assert_eq!(value(argv, "-af"), "atempo=2", "{from:?} -> {to:?}");
            assert!(!has(argv, ["-c:a", "copy"]), "{argv:?}");
            ends_retimed(argv);
        }
    }

    #[test]
    fn a_webm_target_retimes_ahead_of_the_opus_channel_layouts() {
        let plan = sped_plan(Format::Mp4, Format::Webm, &sped("2"), &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert!(value(argv, "-vf").starts_with("setpts=PTS/2"), "{argv:?}");
        assert_eq!(
            value(argv, "-af"),
            format!("atempo=2,{}", registry::OPUS_CHANNEL_LAYOUTS)
        );
        assert!(has(argv, ["-c:a", "libopus"]), "{argv:?}");
    }

    #[test]
    fn a_gif_is_retimed_ahead_of_its_frame_rate_cap() {
        let plan = sped_plan(Format::Mp4, Format::Gif, &sped("3"), &clip_probe(20));
        let argv = &plan.steps[0].argv;
        assert!(
            value(argv, "-vf").starts_with("setpts=PTS/3,fps=15,"),
            "{argv:?}"
        );
        assert!(!argv.iter().any(|a| a == "-af"), "{argv:?}");
        let gif = sped_plan(Format::Gif, Format::Mp4, &sped("3"), &clip_probe(20));
        assert!(value(&gif.steps[0].argv, "-vf").starts_with("setpts=PTS/3,"));
    }

    #[test]
    fn a_stripped_file_sped_into_its_own_format_is_retimed_not_copied() {
        let t = Tuning {
            strip_metadata: true,
            ..sped("2")
        };
        let plan = sped_plan(Format::Mp4, Format::Mp4, &t, &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert!(!has(argv, ["-c", "copy"]), "{argv:?}");
        assert!(value(argv, "-vf").starts_with("setpts=PTS/2,"), "{argv:?}");
        assert!(has(argv, ["-map_metadata", "-1"]), "{argv:?}");
    }

    #[test]
    fn a_cut_and_a_speed_change_cut_the_source_then_retime_it() {
        let t = Tuning {
            speed: Some(crate::speed::parse("2").unwrap()),
            ..ranged(Some("1:00"), Some("1:10"))
        };
        let plan = sped_plan(Format::Mp4, Format::Mp4, &t, &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert_eq!(&argv[..4], ["-ss", "60", "-t", "10"], "{argv:?}");
        assert!(value(argv, "-vf").starts_with("setpts=PTS/2,"), "{argv:?}");
        ends_retimed(argv);
        assert!(plan.range.is_some() && plan.speed.is_some());
    }

    #[test]
    fn a_silent_clip_sped_up_gets_no_audio_filter() {
        let t = Tuning {
            mute: true,
            ..sped("2")
        };
        let plan = sped_plan(Format::Mp4, Format::Mp4, &t, &clip_probe(600));
        let argv = &plan.steps[0].argv;
        assert!(argv.contains(&"-an".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "-af"), "{argv:?}");
    }

    #[test]
    fn shift_pitch_relabels_the_sample_rate() {
        let probe = MediaProbe {
            audio_sample_rate: Some(44_100),
            ..clip_probe(600)
        };
        let t = Tuning {
            shift_pitch: true,
            ..sped("2")
        };
        for to in [Format::M4a, Format::Mp4] {
            let plan = sped_plan(Format::Mp4, to, &t, &probe);
            assert_eq!(
                value(&plan.steps[0].argv, "-af"),
                "aresample=44100,asetrate=88200,aresample=44100",
                "{to:?}"
            );
        }
    }

    #[test]
    fn a_speed_change_is_refused_on_a_still_image() {
        let e = build_tuned(
            Format::Png,
            Format::Jpg,
            &[p("a.png")],
            Path::new("a.jpg"),
            None,
            None,
            &sped("2"),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::InvalidInvocation);
        assert!(
            e.message
                .starts_with("--speed does not apply to png -> jpg"),
            "{}",
            e.message
        );
    }
}
