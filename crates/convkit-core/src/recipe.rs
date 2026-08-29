use std::path::Path;

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
}

/// How a step names its result, which determines what `exec` must do afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        assert_eq!(step.render(&[Path::new("in.docx")], Path::new("out.pdf")), vec!["."]);
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
