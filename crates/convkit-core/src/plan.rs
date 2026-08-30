use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::media;
use crate::probe::MediaProbe;
use crate::resolve::AvailableBackends;
use crate::{registry, Backend, ConvError, ErrorCode, Format, OutputMode, Recipe};

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
    /// step, the intermediate path for every earlier step. Task 9's
    /// execution engine relies on this rather than reading the last argv
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversionPlan {
    pub from: Format,
    pub to: Format,
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub steps: Vec<PlannedStep>,
    pub warnings: Vec<String>,
}

/// Chooses a recipe and renders it. Pure: no filesystem, no process spawning,
/// no executable resolution.
pub fn build(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    probe: Option<&MediaProbe>,
    available: Option<&AvailableBackends>,
) -> Result<ConversionPlan> {
    if inputs.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            "no input files were given",
        ));
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
        if registry::lookup(from, to).is_some() && registry::needs_probe(from, to) {
            // Both constructors self-gate on target shape (compat tables /
            // copyable codecs), so this is a plain preference chain.
            let dynamic = media::stream_mapped_invocation(to, p, &inputs[0], output)
                .or_else(|| media::audio_copy_invocation(from, to, p, &inputs[0], output));
            if let Some(m) = dynamic {
                // Every invocation `media.rs` builds opens with
                // `-i <input>` and closes with the output path, so the
                // path positions (for the Windows long-path rewriter) are
                // exactly argv[1] and the final element.
                let path_args = vec![1, m.argv.len() - 1];
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
                    }],
                    warnings: m.warnings,
                });
            }
        }
    }

    let recipe =
        select(from, to, probe, available).ok_or_else(|| ConvError::unsupported_pair(from, to))?;

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
        } = step.render_full(&inputs_here, &step_outputs[i]);
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
        });
    }

    Ok(ConversionPlan {
        from,
        to,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps,
        warnings: recipe.warnings.iter().map(|w| (*w).to_string()).collect(),
    })
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

    // --- Controller amendments beyond the brief -----------------------------

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
    /// execution engine (Task 9) needs this because reading the last argv
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

    // --- Task 2: availability-based selection for docx/odt -> pdf ----------

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
}
