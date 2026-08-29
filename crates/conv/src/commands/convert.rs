use std::path::{Path, PathBuf};

use convkit_core::{exec, plan, ConvError, ErrorCode, Format};

use crate::cli::Cli;
use crate::render;

/// True when the text after a leading `.` marks the bare-extension shorthand
/// (`.jpg`) rather than an ordinary relative path that merely starts with a
/// dot (`./out.gif`, `.\out.gif`, `..\out.gif`). Classifying on content, not
/// just the leading `.`, is what keeps `conv in.mp4 ./out.gif` writing
/// `out.gif` instead of misparsing the target as an unknown extension
/// `"/out.gif"`. The one case this gives up is converting to a file
/// literally named `.hidden`, where the user must write `./.hidden`.
fn is_bare_extension_shorthand(rest: &str) -> bool {
    !rest.contains('/') && !rest.contains('\\') && !rest.contains('.')
}

/// Resolves the `IN OUT` and `IN .ext` positional forms. Batch forms arrive in
/// Task 12.
pub fn resolve_pair(paths: &[PathBuf]) -> Result<(PathBuf, PathBuf), ConvError> {
    let [input, target] = paths else {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            "expected an input and an output, e.g. `conv in.mp4 out.gif`",
        ));
    };
    let t = target.to_string_lossy();
    if let Some(rest) = t.strip_prefix('.') {
        if is_bare_extension_shorthand(rest) {
            if Format::from_ext(rest).is_none() {
                return Err(ConvError::unknown_format(rest));
            }
            return Ok((input.clone(), input.with_extension(rest)));
        }
    }
    Ok((input.clone(), target.clone()))
}

fn format_of(p: &Path) -> Result<Format, ConvError> {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    Format::from_ext(ext).ok_or_else(|| ConvError::unknown_format(ext))
}

pub fn run(cli: &Cli) -> i32 {
    match execute(cli) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            if cli.json {
                let envelope = serde_json::json!({ "ok": false, "error": e });
                eprintln!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                eprint!("{}", render::error_human(&e));
            }
            e.code.exit_code()
        }
    }
}

fn execute(cli: &Cli) -> Result<String, ConvError> {
    let (input, output) = resolve_pair(&cli.paths)?;
    let from = format_of(&input)?;
    let to = format_of(&output)?;

    if cli.dry_run {
        let p = plan::build(from, to, &[input], &output, None)?;
        return Ok(if cli.json {
            format!(
                "{}\n",
                serde_json::to_string_pretty(&render::plan_json(&p)).unwrap()
            )
        } else {
            render::plan_human(&p)
        });
    }

    if output.exists() && !cli.overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", output.display()),
        ));
    }

    let req = exec::Request {
        from,
        to,
        inputs: vec![input],
        output,
    };
    let outcome = exec::run(&req, &cli.resolver(), &mut |_| {})?;
    Ok(if cli.json {
        format!(
            "{}\n",
            serde_json::to_string_pretty(&render::outcome_json(&outcome)).unwrap()
        )
    } else {
        render::outcome_human(&outcome)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn bare_extension_shorthand_derives_the_output_name() {
        let (input, output) = resolve_pair(&[p("photo.heic"), p(".jpg")]).unwrap();
        assert_eq!(input, p("photo.heic"));
        assert_eq!(output, p("photo.jpg"));
    }

    #[test]
    fn bare_extension_shorthand_typo_still_suggests_a_correction() {
        let e = resolve_pair(&[p("in.mp4"), p(".gff")]).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownFormat);
        assert!(e.message.contains("did you mean"), "{}", e.message);
    }

    #[test]
    fn dot_slash_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p("./out.gif")]).unwrap();
        assert_eq!(output, p("./out.gif"));
    }

    #[test]
    fn dot_backslash_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p(r".\out.gif")]).unwrap();
        assert_eq!(output, p(r".\out.gif"));
    }

    #[test]
    fn parent_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p(r"..\out.gif")]).unwrap();
        assert_eq!(output, p(r"..\out.gif"));
    }
}
