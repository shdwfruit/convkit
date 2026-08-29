use std::path::Path;

use serde::Serialize;

use crate::Backend;

/// A single argument slot in a backend invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// A literal flag or value, passed through verbatim.
    Lit(&'static str),
    /// The first (usually only) input path.
    Input,
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
        let mut out = Vec::with_capacity(self.args.len());
        for arg in self.args {
            match arg {
                Arg::Lit(s) => out.push((*s).to_string()),
                Arg::Input => out.push(inputs[0].to_string_lossy().into_owned()),
                Arg::Inputs => out.extend(inputs.iter().map(|p| p.to_string_lossy().into_owned())),
                Arg::Output => out.push(output.to_string_lossy().into_owned()),
                Arg::OutDir => {
                    let dir = output.parent().filter(|p| !p.as_os_str().is_empty());
                    out.push(match dir {
                        Some(d) => d.to_string_lossy().into_owned(),
                        None => ".".to_string(),
                    });
                }
                Arg::BackendPath(backend) => out.push(backend.path_placeholder()),
            }
        }
        out
    }
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
        let argv = GIF.render(&[Path::new("in.mp4")], Path::new("out.gif"));
        assert_eq!(argv, vec!["-i", "in.mp4", "-y", "out.gif"]);
    }

    #[test]
    fn out_dir_mode_renders_the_parent_directory() {
        let step = Step {
            backend: Backend::Soffice,
            args: &[Arg::Lit("--outdir"), Arg::OutDir, Arg::Input],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        };
        let argv = step.render(&[Path::new("a/in.docx")], Path::new("b/out.pdf"));
        assert_eq!(argv, vec!["--outdir", "b", "a/in.docx"]);
    }

    #[test]
    fn out_dir_of_a_bare_filename_is_the_current_directory() {
        let step = Step {
            backend: Backend::Soffice,
            args: &[Arg::OutDir],
            output: OutputMode::OutDir,
            intermediate_ext: None,
        };
        assert_eq!(
            step.render(&[Path::new("in.docx")], Path::new("out.pdf")),
            vec!["."]
        );
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
}
