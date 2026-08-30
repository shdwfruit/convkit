use std::path::Path;

use serde::Serialize;

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
}

impl Tuning {
    pub fn is_empty(&self) -> bool {
        self.resize.is_none() && self.quality.is_none() && self.colors.is_none()
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
    /// `-resize <geometry>` when `--resize` was given; renders *nothing*
    /// otherwise, keeping untuned argv byte-identical to the static table.
    TuneResize,
    /// `-colors <n>` when `--colors` was given; renders nothing otherwise.
    TuneColors,
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
    /// through. The validation boundary is `plan::build` (Task 7), the
    /// public entry point every caller goes through, which rejects empty
    /// inputs with a typed `ConvError` before any `Step` is ever rendered.
    pub fn render(&self, inputs: &[&Path], output: &Path) -> Vec<String> {
        self.render_full(inputs, output, &Tuning::default()).argv
    }

    /// `render` plus the positions of the tokens that are filesystem paths.
    ///
    /// Only the executor needs the positions; every other caller -- the
    /// registry's snapshot tests, `--dry-run`'s renderer -- wants the command
    /// line alone, which is why `render` stays the short spelling and
    /// delegates here rather than the two walking `args` separately and
    /// drifting apart.
    pub fn render_full(&self, inputs: &[&Path], output: &Path, tuning: &Tuning) -> Rendered {
        let mut argv = Vec::with_capacity(self.args.len());
        let mut path_args = Vec::new();
        for arg in self.args {
            match arg {
                Arg::Lit(s) => argv.push((*s).to_string()),
                Arg::Quality(default) => argv.push(match tuning.quality {
                    Some(q) => q.to_string(),
                    None => (*default).to_string(),
                }),
                Arg::TuneResize => {
                    if let Some(g) = &tuning.resize {
                        argv.push("-resize".to_string());
                        argv.push(g.clone());
                    }
                }
                Arg::TuneColors => {
                    if let Some(n) = tuning.colors {
                        argv.push("-colors".to_string());
                        argv.push(n.to_string());
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
        };
        let r = TUNABLE.render_full(&[Path::new("in.png")], Path::new("out.jpg"), &tuning);
        assert_eq!(
            r.argv,
            vec!["in.png", "-resize", "1600x900", "-colors", "64", "-quality", "70", "out.jpg"]
        );
    }

    /// Path positions must stay correct when tune slots expand: the output
    /// token's recorded index shifts with the inserted pairs.
    #[test]
    fn path_positions_track_tune_slot_expansion() {
        let tuning = Tuning {
            resize: Some("50%".into()),
            ..Tuning::default()
        };
        let r = TUNABLE.render_full(&[Path::new("in.png")], Path::new("out.jpg"), &tuning);
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
}
