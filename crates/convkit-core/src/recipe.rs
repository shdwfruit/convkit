use std::path::Path;

use serde::Serialize;

use crate::video::ResolvedVideo;
use crate::Backend;

/// User-facing tuning for one invocation — the first parameter surface in
/// a DSL that deliberately had none. Every field is optional and `None`
/// renders *exactly* the registry's static default, so an untuned run's
/// argv is byte-identical to the snapshot table; the fields only exist
/// where a recipe declares a matching slot (`Arg::Quality`,
/// `Arg::TuneResize`, `Arg::TuneColors`), and `plan::build_tuned` refuses
/// a flag whose slot the selected recipe doesn't carry rather than
/// silently ignoring it.
///
/// Values arrive pre-validated by the CLI (geometry charset, numeric
/// ranges); this struct is dumb data, not a validator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tuning {
    /// ImageMagick-style geometry: `W`, `WxH` (fits within, aspect
    /// preserved), `Wx`, `xH`, or `N%`.
    pub resize: Option<String>,
    /// 1–100, for lossy image targets and image→PDF.
    pub quality: Option<u8>,
    /// 2–256, palette reduction on raster targets.
    pub colors: Option<u16>,
    /// Frame-rate cap for video and GIF targets: `24`, `29.97`, or
    /// `30000/1001`. A cap, never a floor -- see `video::resolve`.
    ///
    /// Held as the user's own string, not a parsed rational: the exact
    /// text reaches ffmpeg, so `30000/1001` stays exact in the argv.
    pub fps: Option<String>,
    /// Constant-quality anchor for video targets. Unlike the two geometry
    /// knobs this is not clamped against the source: it is an anchor, not
    /// a bound.
    pub crf: Option<u8>,
    /// A size ceiling for video targets. Not a knob but a policy: when set,
    /// `plan::build_tuned` hands the conversion to `sized::plan`, which
    /// chooses the resolution, frame rate and bitrates itself.
    pub max_size: Option<crate::size::MaxSize>,
    /// Lets `resize` enlarge a picture smaller than its geometry. Without
    /// it, `resize` only ever fits within, on every target.
    pub upscale: bool,
    /// Removes the source's metadata except the colour profile, the
    /// orientation and an audio file's content tags: see `metadata`.
    pub strip_metadata: bool,
    /// The part of the source to keep (`--start`/`--end`/`--duration`), as
    /// typed. Resolved against the probe by `trim::resolve`; the cut reaches
    /// argv through `ResolvedVideo::cut`.
    pub range: Option<crate::trim::Range>,
    /// Drop every audio track: a silent clip, as `conv trim` writes for a
    /// cut made on its video bar alone. Video targets only.
    pub mute: bool,
}

impl Tuning {
    pub fn is_empty(&self) -> bool {
        !self.upscale
            && !self.strip_metadata
            && !self.mute
            && self.range.is_none()
            && self.resize.is_none()
            && self.quality.is_none()
            && self.colors.is_none()
            && self.fps.is_none()
            && self.crf.is_none()
            && self.max_size.is_none()
    }
}

/// How a recipe spells its width cap, and what it scales with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleStyle {
    /// No authored scale. A tuned cap composes in front of `tail`, which
    /// for every libx264 target is the even-dimension guard.
    Guarded,
    /// GIF's capped lanczos downscale, whose width the tuning overrides.
    /// Needs no even guard, for one reason and not two: GIF has no
    /// yuv420p constraint at all. (`h=-2` appears in only one of the four
    /// geometry forms, so it cannot be the reason.)
    CappedLanczos { default_width: &'static str },
}

/// The parts of one `-vf` value, so it can be composed at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoChainSpec {
    /// Filters that must run before the tuned head. Empty for every recipe
    /// but the GIF tonemap sibling, whose HDR->SDR mapping has to see the
    /// source signal before anything decimates or resamples it. Carries its
    /// own trailing comma.
    pub prefix: &'static str,
    /// The frame-rate cap this recipe authored. GIF authors "15"; the
    /// transcodes carry the source rate and author `None`. Any `--fps`
    /// replaces it, including one the source already satisfies.
    pub fps: Option<&'static str>,
    /// How this recipe spells a width cap.
    pub scale: ScaleStyle,
    /// Everything after the tuned head, verbatim.
    pub tail: &'static str,
}

impl VideoChainSpec {
    /// Builds the one `-vf` value.
    ///
    /// Untuned, this reproduces byte for byte the constant the
    /// `Arg::VideoChain` variant replaced -- which is not a property to be
    /// checked afterwards but the reason the struct is shaped head + tail.
    /// It is what keeps `tests/recipes.rs`'s snapshot green.
    pub fn compose(&self, resolved: &ResolvedVideo) -> String {
        let mut out = String::from(self.prefix);
        let authored = self.fps.filter(|_| !resolved.keep_source_rate);
        if let Some(f) = resolved.fps.as_deref().or(authored) {
            out.push_str("fps=");
            out.push_str(f);
            out.push(',');
        }
        match (&resolved.scale, &self.scale) {
            // A user width must not silently downgrade the resampler the
            // recipe chose.
            (Some(s), ScaleStyle::CappedLanczos { .. }) => {
                out.push_str(s);
                out.push_str(":flags=lanczos,");
            }
            (Some(s), ScaleStyle::Guarded) => {
                out.push_str(s);
                out.push(',');
            }
            // `--resize` replaced the default width with the source's own.
            (None, ScaleStyle::CappedLanczos { .. }) if resolved.keep_source_size => {}
            (None, ScaleStyle::CappedLanczos { default_width }) => {
                out.push_str(&format!(
                    r"scale=w=min({default_width}\,iw):h=-2:flags=lanczos,"
                ));
            }
            (None, ScaleStyle::Guarded) => {}
        }
        out.push_str(self.tail);
        out
    }
}

/// A single argument slot in a backend invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// A literal flag or value, passed through verbatim.
    Lit(&'static str),
    /// The quality value: the user's `--quality` override when given, the
    /// carried registry default otherwise. Spelled with its default so the
    /// number stays authored next to the recipe (`Arg::Quality("92")`),
    /// not buried in the renderer.
    Quality(&'static str),
    /// The CRF value: the user's `--crf` override when given, the carried
    /// registry anchor otherwise. Spelled with its anchor for the same
    /// reason `Quality` is -- the number stays authored beside the recipe
    /// (`Arg::Crf("20")`) rather than buried in the renderer.
    Crf(&'static str),
    /// The `-vf` value, composed at render time.
    ///
    /// ffmpeg keeps a single filter chain per output stream, so a second
    /// `-vf` replaces the first rather than chaining onto it; a knob cannot
    /// append its own filter, and every chain must be built as one string.
    /// Holds a reference because `Arg` is `Copy`.
    VideoChain(&'static VideoChainSpec),
    /// `-resize <geometry>` when `--resize` was given; renders *nothing*
    /// otherwise, keeping untuned argv byte-identical to the static table.
    TuneResize,
    /// `-colors <n>` when `--colors` was given; renders nothing otherwise.
    TuneColors,
    /// `--strip-metadata`'s arguments for this step's backend when the flag
    /// was given (`metadata::MAGICK_STRIP` or `metadata::ffmpeg_args`);
    /// renders nothing otherwise, keeping untuned argv byte-identical.
    /// Authored only on ImageMagick and ffmpeg steps.
    StripMetadata,
    /// The cut, as ffmpeg input options (`-ss S -t D`), when a range was
    /// given; renders *nothing* otherwise, keeping untuned argv
    /// byte-identical to the static table. Authored first in every ffmpeg
    /// recipe, ahead of `-i`, because these are input options: see
    /// `trim::Cut::input_args`. A step with this slot also gets the cut's
    /// output options just before its `Output` (`trim::Cut::output_args`).
    Trim,
    /// `-an` for a silent clip; renders nothing otherwise. An output
    /// option, so authored just before the output in the video recipes. It
    /// drops audio even where a recipe maps it explicitly (`-map 0`),
    /// checked on ffmpeg 6.1 and 9.0.
    Mute,
    /// The first (usually only) input path.
    Input,
    /// The first input path with ImageMagick's `[0]` frame selector
    /// appended: read only the first frame/page. The explicit frame policy
    /// for single-image targets — without it, a multi-page TIFF or
    /// animated WebP into jpg/png/bmp makes magick write `stem-0.jpg`,
    /// `stem-1.jpg`, … and the conversion fails with an empty "produced no
    /// output". Harmless on single-frame sources.
    InputFirstFrame,
    /// The directory containing the first input path (`.` for a bare
    /// filename). For backends like `pandoc` that resolve a document's
    /// relative resources (images) against a search path rather than
    /// against the document's own location — without `--resource-path
    /// <this>`, `conv docs/readme.md out.docx` run from anywhere but
    /// `docs/` silently dropped every image.
    InputDir,
    /// Every input path, in order. Used by the image→PDF merge recipe.
    Inputs,
    /// The output path this step writes.
    Output,
    /// The directory containing the output path. For backends like `soffice`
    /// that take `--outdir` and name the file themselves.
    OutDir,
    /// A resolved backend's absolute path, substituted in by `exec::run` at
    /// execution time. `render` (and therefore `plan::build`) can't know the
    /// real path — that requires filesystem access `plan::build` must never
    /// perform — so this renders `Backend::path_placeholder` instead: a
    /// fixed, readable stand-in `--dry-run` can show honestly.
    ///
    /// This is deliberately a plain value substitution, not a formatted one:
    /// unlike `plan::USER_INSTALLATION_PLACEHOLDER` (a fixed *position* —
    /// always argv[0] of a `Soffice` step, prepended by `plan::build` itself
    /// outside any recipe's own `args`, and substituted with a per-run
    /// *formatted* `-env:UserInstallation=<url>` string derived from a
    /// scratch profile directory that has nothing to do with any backend's
    /// own executable path), `BackendPath` is authored directly in a
    /// recipe's own `args`, can sit anywhere in argv, and is always
    /// substituted with exactly the named backend's resolved absolute path,
    /// verbatim. The two needs looked alike (both are "argv content only
    /// known at execution time") but have different shapes, so they stay
    /// separate mechanisms rather than one forced into the other.
    BackendPath(Backend),
}

/// How a step names its result, which determines what `exec` must do afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// The step writes exactly the path given to it.
    Path,
    /// The step writes *some* file into the given directory and chooses the
    /// name itself; exec must locate it and move it into place.
    OutDir,
    /// The step writes nothing the executor keeps: ffmpeg's first pass,
    /// whose only product is the pass log. `exec` checks its exit status
    /// but not for an output file.
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub backend: Backend,
    pub args: &'static [Arg],
    pub output: OutputMode,
    /// For all but the final step: the extension of the intermediate file this
    /// step produces. `None` on the final step.
    pub intermediate_ext: Option<&'static str>,
}

/// A conversion, as data. Multi-step recipes are hardcoded pipelines, not a
/// routing graph — see the spec's non-goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub steps: &'static [Step],
    /// Fidelity caveats surfaced to the user. Core never prints these; they
    /// travel on the result and the frontend renders them.
    pub warnings: &'static [&'static str],
}

impl Step {
    /// Render this step's argv. Paths are rendered lossily via `to_string_lossy`
    /// for display and snapshotting; `exec` passes real `OsStr` values.
    ///
    /// # Preconditions
    ///
    /// `inputs` must be non-empty whenever this step's `args` contain
    /// `Arg::Input` or `Arg::Inputs` — `Arg::Input` indexes `inputs[0]`
    /// unchecked and will panic on an empty slice. This function does not
    /// validate that; it stays a pure formatter with no `Result` to thread
    /// through. The validation boundary is `plan::build`, the
    /// public entry point every caller goes through, which rejects empty
    /// inputs with a typed `ConvError` before any `Step` is ever rendered.
    pub fn render(&self, inputs: &[&Path], output: &Path) -> Vec<String> {
        self.render_full(
            inputs,
            output,
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        )
        .argv
    }

    /// `render` plus the positions of the tokens that are filesystem paths.
    ///
    /// Only the executor needs the positions; every other caller -- the
    /// registry's snapshot tests, `--dry-run`'s renderer -- wants the command
    /// line alone, which is why `render` stays the short spelling and
    /// delegates here rather than the two walking `args` separately and
    /// drifting apart.
    ///
    /// `kept` is the tags `--strip-metadata` writes back on an ffmpeg step:
    /// the probe's `kept_tags`, or none without a probe.
    pub fn render_full(
        &self,
        inputs: &[&Path],
        output: &Path,
        tuning: &Tuning,
        video: &ResolvedVideo,
        kept: &[(String, String)],
    ) -> Rendered {
        let mut argv = Vec::with_capacity(self.args.len());
        let mut path_args = Vec::new();
        for arg in self.args {
            match arg {
                Arg::Lit(s) => argv.push((*s).to_string()),
                Arg::Quality(default) => argv.push(match tuning.quality {
                    Some(q) => q.to_string(),
                    None => (*default).to_string(),
                }),
                Arg::Crf(default) => argv.push(match tuning.crf {
                    Some(n) => n.to_string(),
                    None => (*default).to_string(),
                }),
                Arg::VideoChain(spec) => argv.push(spec.compose(video)),
                Arg::TuneResize => {
                    if let Some(g) = &tuning.resize {
                        argv.push("-resize".to_string());
                        // ImageMagick's `>`: only shrink a larger image.
                        // `--upscale` drops it, and the plan warns.
                        argv.push(if tuning.upscale {
                            g.clone()
                        } else {
                            format!("{g}>")
                        });
                    }
                }
                Arg::TuneColors => {
                    if let Some(n) = tuning.colors {
                        argv.push("-colors".to_string());
                        argv.push(n.to_string());
                    }
                }
                Arg::StripMetadata if tuning.strip_metadata => match self.backend {
                    Backend::Magick => argv.extend(
                        crate::metadata::MAGICK_STRIP
                            .iter()
                            .map(|s| (*s).to_string()),
                    ),
                    Backend::Ffmpeg => argv.extend(crate::metadata::ffmpeg_args(kept)),
                    other => unreachable!("--strip-metadata slot authored on a {other:?} step"),
                },
                Arg::StripMetadata => {}
                Arg::Trim => {
                    if let Some(cut) = &video.cut {
                        argv.extend(cut.input_args());
                    }
                }
                Arg::Mute => {
                    if tuning.mute {
                        argv.push("-an".to_string());
                    }
                }
                Arg::Input => {
                    path_args.push(argv.len());
                    argv.push(inputs[0].to_string_lossy().into_owned());
                }
                Arg::InputFirstFrame => {
                    // magick's frame selector rides on the token itself
                    // (`photo.tiff[0]`), and this *is* a path for the
                    // Windows long-path rewriter's purposes. Leaving it out
                    // of `path_args` left the most common image conversions
                    // -- everything reaching `IMG_TO_JPG` -- still failing
                    // past MAX_PATH while every other recipe was fixed.
                    //
                    // The two worries that argue against including it both
                    // turn out not to hold, checked rather than reasoned
                    // about. `std::path::absolute` never touches the
                    // filesystem, so a name ending in `[0]` is carried
                    // through as an ordinary file name rather than resolved
                    // and lost; and magick parses the selector off the end
                    // of a verbatim path exactly as it does a plain one --
                    // `magick "\\?\C:\...\p.heic[0]" out.jpg` exits 0 on a
                    // 270-character input that fails without the prefix.
                    path_args.push(argv.len());
                    argv.push(format!("{}[0]", inputs[0].to_string_lossy()));
                }
                Arg::InputDir => {
                    let dir = inputs[0].parent().filter(|p| !p.as_os_str().is_empty());
                    path_args.push(argv.len());
                    argv.push(match dir {
                        Some(d) => d.to_string_lossy().into_owned(),
                        None => ".".to_string(),
                    });
                }
                Arg::Inputs => {
                    for input in inputs {
                        path_args.push(argv.len());
                        argv.push(input.to_string_lossy().into_owned());
                    }
                }
                Arg::Output => {
                    if let Some(cut) = video
                        .cut
                        .as_ref()
                        .filter(|_| self.args.contains(&Arg::Trim))
                    {
                        argv.extend(cut.output_args());
                    }
                    path_args.push(argv.len());
                    argv.push(output.to_string_lossy().into_owned());
                }
                Arg::OutDir => {
                    let dir = output.parent().filter(|p| !p.as_os_str().is_empty());
                    path_args.push(argv.len());
                    argv.push(match dir {
                        Some(d) => d.to_string_lossy().into_owned(),
                        None => ".".to_string(),
                    });
                }
                // Not a filesystem path in the sense `path_args` means: it is
                // a placeholder the executor swaps for a resolved executable,
                // and rewriting it as a path would break that substitution.
                Arg::BackendPath(backend) => argv.push(backend.path_placeholder()),
            }
        }
        Rendered { argv, path_args }
    }
}

/// One step's rendered command line, plus the positions in it that hold
/// filesystem paths.
///
/// The positions exist because the executor has to be able to rewrite paths
/// -- and only paths -- without re-deriving which tokens are which. On
/// Windows a path close to `MAX_PATH` has to be handed over in extended
/// (`\\?\`) form or the backend fails with an error naming the wrong cause
/// (F193), and a heuristic over rendered strings would eventually rewrite a
/// filter graph or a codec name. Recording the positions at the one moment
/// they are known for certain costs nothing and cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub argv: Vec<String>,
    /// Indices into `argv`, ascending.
    pub path_args: Vec<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Backend;
    use std::path::Path;

    const GIF: Step = Step {
        backend: Backend::Ffmpeg,
        args: &[Arg::Lit("-i"), Arg::Input, Arg::Lit("-y"), Arg::Output],
        output: OutputMode::Path,
        intermediate_ext: None,
    };

    #[test]
    fn renders_positional_input_and_output() {
        let r = GIF.render_full(
            &[Path::new("in.mp4")],
            Path::new("out.gif"),
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(r.argv, vec!["-i", "in.mp4", "-y", "out.gif"]);
        assert_eq!(
            r.path_args,
            vec![1, 3],
            "the input and the output, not the flags"
        );
    }

    /// The frame selector is part of the token, but the token is still a
    /// path: excluding it from `path_args` is what left `heic -> jpg` and
    /// every other `IMG_TO_JPG` conversion failing past MAX_PATH on Windows
    /// after the rest of the long-path fix landed.
    #[test]
    fn the_first_frame_selector_is_still_a_path_position() {
        let step = Step {
            backend: Backend::Magick,
            args: &[Arg::InputFirstFrame, Arg::Lit("-auto-orient"), Arg::Output],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let r = step.render_full(
            &[Path::new("photo.heic")],
            Path::new("out.jpg"),
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(r.argv[0], "photo.heic[0]");
        assert_eq!(
            r.path_args,
            vec![0, 2],
            "the selector token and the output, not the flag"
        );
    }

    #[test]
    fn out_dir_mode_renders_the_parent_directory() {
        let step = Step {
            backend: Backend::Soffice,
            args: &[Arg::Lit("--outdir"), Arg::OutDir, Arg::Input],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        };
        let r = step.render_full(
            &[Path::new("a/in.docx")],
            Path::new("b/out.pdf"),
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(r.argv, vec!["--outdir", "b", "a/in.docx"]);
        assert_eq!(r.path_args, vec![1, 2], "the out-dir and the input");
    }

    #[test]
    fn out_dir_of_a_bare_filename_is_the_current_directory() {
        let step = Step {
            backend: Backend::Soffice,
            args: &[Arg::OutDir],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        };
        let r = step.render_full(
            &[Path::new("in.docx")],
            Path::new("out.pdf"),
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(r.argv, vec!["."]);
        assert_eq!(r.path_args, vec![0]);
    }

    #[test]
    fn backend_path_renders_a_readable_placeholder_not_a_real_path() {
        let step = Step {
            backend: Backend::Pandoc,
            args: &[
                Arg::Input,
                Arg::Lit("--pdf-engine"),
                Arg::BackendPath(Backend::Typst),
                Arg::Lit("-o"),
                Arg::Output,
            ],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let argv = step.render(&[Path::new("in.docx")], Path::new("out.pdf"));
        assert_eq!(
            argv,
            vec![
                "in.docx",
                "--pdf-engine",
                "<resolved typst path>",
                "-o",
                "out.pdf"
            ]
        );
    }

    #[test]
    fn inputs_expands_to_every_input_in_order() {
        let step = Step {
            backend: Backend::Magick,
            args: &[Arg::Inputs, Arg::Output],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let argv = step.render(
            &[Path::new("a.png"), Path::new("b.png")],
            Path::new("out.pdf"),
        );
        assert_eq!(argv, vec!["a.png", "b.png", "out.pdf"]);
    }

    // --- Tuning slots: the first parameter surface in the DSL ------------

    const TUNABLE: Step = Step {
        backend: Backend::Magick,
        args: &[
            Arg::Input,
            Arg::TuneResize,
            Arg::TuneColors,
            Arg::Lit("-quality"),
            Arg::Quality("92"),
            Arg::Output,
        ],
        output: OutputMode::Path,
        intermediate_ext: None,
    };

    /// The load-bearing property: an empty `Tuning` renders *byte-identical*
    /// argv to the static table — TuneResize/TuneColors vanish, Quality
    /// falls back to its carried default. This is what keeps the 115-pair
    /// snapshot stable.
    #[test]
    fn empty_tuning_renders_exactly_the_static_default_argv() {
        let argv = TUNABLE.render(&[Path::new("in.png")], Path::new("out.jpg"));
        assert_eq!(argv, vec!["in.png", "-quality", "92", "out.jpg"]);
    }

    #[test]
    fn tuning_fills_its_slots_and_only_its_slots() {
        let tuning = Tuning {
            resize: Some("1600x900".into()),
            quality: Some(70),
            colors: Some(64),
            fps: None,
            crf: None,
            max_size: None,
            upscale: false,
            strip_metadata: false,
            range: None,
            mute: false,
        };
        let r = TUNABLE.render_full(
            &[Path::new("in.png")],
            Path::new("out.jpg"),
            &tuning,
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(
            r.argv,
            vec![
                "in.png",
                "-resize",
                "1600x900>",
                "-colors",
                "64",
                "-quality",
                "70",
                "out.jpg"
            ]
        );
    }

    /// ImageMagick's `>` flag is what keeps `-resize` from enlarging, so it
    /// is there unless `--upscale` asked for that.
    #[test]
    fn an_image_resize_only_enlarges_with_upscale() {
        let render = |upscale| {
            TUNABLE
                .render_full(
                    &[Path::new("in.png")],
                    Path::new("out.jpg"),
                    &Tuning {
                        resize: Some("50%".into()),
                        upscale,
                        ..Tuning::default()
                    },
                    &ResolvedVideo::default(),
                    &[],
                )
                .argv
        };
        assert!(render(false).windows(2).any(|w| w == ["-resize", "50%>"]));
        assert!(render(true).windows(2).any(|w| w == ["-resize", "50%"]));
    }

    /// Path positions must stay correct when tune slots expand: the output
    /// token's recorded index shifts with the inserted pairs.
    #[test]
    fn path_positions_track_tune_slot_expansion() {
        let tuning = Tuning {
            resize: Some("50%".into()),
            ..Tuning::default()
        };
        let r = TUNABLE.render_full(
            &[Path::new("in.png")],
            Path::new("out.jpg"),
            &tuning,
            &ResolvedVideo::default(),
            &[],
        );
        for &i in &r.path_args {
            assert!(
                r.argv[i] == "in.png" || r.argv[i] == "out.jpg",
                "position {i} points at non-path token {:?} in {:?}",
                r.argv[i],
                r.argv
            );
        }
        assert_eq!(r.path_args.len(), 2, "{:?}", r.path_args);
    }

    #[test]
    fn every_tuning_field_on_its_own_makes_the_struct_non_empty() {
        // is_empty() is the early-return guard in BOTH validators
        // (plan.rs:219, :243). A field missing from it does not weaken
        // validation for that field -- it disables validation entirely,
        // turning a refusal into the silent no-op the project refuses.
        // This test is the only thing standing between a new field and
        // that bug; the compiler will not object.
        let each: Vec<Tuning> = vec![
            Tuning {
                resize: Some("640x480".into()),
                ..Default::default()
            },
            Tuning {
                quality: Some(80),
                ..Default::default()
            },
            Tuning {
                colors: Some(64),
                ..Default::default()
            },
            Tuning {
                fps: Some("24".into()),
                ..Default::default()
            },
            Tuning {
                crf: Some(28),
                ..Default::default()
            },
            Tuning {
                range: crate::trim::Range::new(
                    Some(crate::trim::parse_time("5").unwrap()),
                    None,
                    None,
                )
                .unwrap(),
                ..Default::default()
            },
            Tuning {
                mute: true,
                ..Default::default()
            },
        ];
        assert!(Tuning::default().is_empty());
        for t in &each {
            assert!(!t.is_empty(), "is_empty() does not know about {t:?}");
        }
        // If a field is added without extending this list, this catches it.
        assert_eq!(
            each.len(),
            7,
            "Tuning gained a field; add it to `each` and to is_empty()"
        );
    }

    #[test]
    fn a_mute_slot_renders_an_only_for_a_silent_clip() {
        let step = Step {
            backend: Backend::Ffmpeg,
            args: &[Arg::Lit("-i"), Arg::Input, Arg::Mute, Arg::Output],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let render = |mute| {
            step.render_full(
                &[Path::new("in.avi")],
                Path::new("out.mp4"),
                &Tuning {
                    mute,
                    ..Tuning::default()
                },
                &ResolvedVideo::default(),
                &[],
            )
            .argv
        };
        assert_eq!(render(false), ["-i", "in.avi", "out.mp4"]);
        assert_eq!(render(true), ["-i", "in.avi", "-an", "out.mp4"]);
    }

    #[test]
    fn a_trim_slot_renders_the_cut_before_the_input_and_nothing_without_one() {
        let step = Step {
            backend: Backend::Ffmpeg,
            args: &[Arg::Trim, Arg::Lit("-i"), Arg::Input, Arg::Output],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let untuned = step.render(&[Path::new("in.mp4")], Path::new("out.mp4"));
        assert_eq!(untuned, ["-i", "in.mp4", "out.mp4"]);
        let video = ResolvedVideo {
            cut: Some(crate::trim::Cut {
                start_ms: 62_000,
                end_ms: Some(70_000),
            }),
            ..ResolvedVideo::default()
        };
        let r = step.render_full(
            &[Path::new("in.mp4")],
            Path::new("out.mp4"),
            &Tuning::default(),
            &video,
            &[],
        );
        assert_eq!(
            r.argv,
            [
                "-ss",
                "62",
                "-t",
                "8",
                "-i",
                "in.mp4",
                "-map_chapters",
                "-1",
                "-copypriorss:s",
                "0",
                "out.mp4"
            ]
        );
        assert_eq!(r.path_args, [5, 10], "the input and output, not the times");
    }

    #[test]
    fn an_untuned_crf_slot_renders_its_authored_anchor() {
        let step = Step {
            backend: Backend::Ffmpeg,
            args: &[Arg::Lit("-crf"), Arg::Crf("20")],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let out = step.render_full(
            &[Path::new("in.mp4")],
            Path::new("out.mp4"),
            &Tuning::default(),
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(out.argv, vec!["-crf".to_string(), "20".to_string()]);
    }

    #[test]
    fn a_tuned_crf_slot_renders_the_users_value() {
        let step = Step {
            backend: Backend::Ffmpeg,
            args: &[Arg::Lit("-crf"), Arg::Crf("20")],
            output: OutputMode::Path,
            intermediate_ext: None,
        };
        let out = step.render_full(
            &[Path::new("in.mp4")],
            Path::new("out.mp4"),
            &Tuning {
                crf: Some(28),
                ..Default::default()
            },
            &ResolvedVideo::default(),
            &[],
        );
        assert_eq!(out.argv, vec!["-crf".to_string(), "28".to_string()]);
    }

    #[test]
    fn a_size_target_alone_makes_tuning_non_empty() {
        let t = Tuning {
            max_size: Some(crate::size::parse("10mb").unwrap()),
            ..Default::default()
        };
        assert!(
            !t.is_empty(),
            "the validators' early return would skip --max-size"
        );
    }

    #[test]
    fn discard_serialises_in_the_plan_envelope_spelling() {
        assert_eq!(
            serde_json::to_string(&OutputMode::Discard).unwrap(),
            "\"discard\""
        );
    }
}
