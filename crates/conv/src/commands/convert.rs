use std::path::{Path, PathBuf};

use convkit_core::{exec, plan, ConvError, ErrorCode, Format};

use crate::cli::Cli;
use crate::render;

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
    if let Some(ext) = t.strip_prefix('.') {
        if Format::from_ext(ext).is_none() {
            return Err(ConvError::unknown_format(ext));
        }
        return Ok((input.clone(), input.with_extension(ext)));
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
        return Ok(render::plan_human(&p));
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
    Ok(render::outcome_human(&outcome))
}
