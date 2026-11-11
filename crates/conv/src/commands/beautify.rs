//! `conv beautify SHOT [OUT] [-o DIR]`: a screenshot on a background, with
//! padding, rounded corners and a shadow, drawn by convkit itself.
//!
//! The paths work as in a conversion: without OUT the result goes beside
//! the input as NAME-beautified.png, a bare `.ext` picks the format, and
//! `-o` is the directory to write into. A picture pasted from the
//! clipboard has no folder of its own, so its result is written here.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::beautify::{self, Background, Plan, Style};
use convkit_core::{registry, ConvError, ErrorCode, Format, Kind, Outcome};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::commands::convert;
use crate::{input, output, paste, picture, render};

/// The name a result gets after its input's.
const SUFFIX: &str = "-beautified";

pub fn run(cli: &Cli) -> i32 {
    match beautify(cli) {
        Ok(()) => 0,
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

fn beautify(cli: &Cli) -> Result<(), ConvError> {
    let Some(Command::Beautify {
        input,
        output,
        outdir,
        bg,
        padding,
        radius,
        no_shadow,
        shadow,
        overwrite,
        copy,
        paste,
        dry_run,
    }) = &cli.command
    else {
        unreachable!("main dispatches only Command::Beautify here");
    };
    let start = Instant::now();
    // With --paste the one path given is OUT.
    let (named, output) =
        match (*paste, input) {
            (true, Some(_)) if output.is_some() => return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "--paste takes the screenshot from the clipboard; give at most the file to write",
            )),
            (true, Some(out)) => (None, Some(out.clone())),
            _ => (input.clone(), output.clone()),
        };
    // The folder a pasted picture is in lives until the result is written.
    let pasted = paste.then(|| paste::one("conv beautify")).transpose()?;
    let (source, beside) = match (&pasted, &named) {
        (Some(p), _) if p.homeless() => (p.path.clone(), None),
        (Some(p), _) => (p.path.clone(), p.path.parent().map(Path::to_path_buf)),
        (None, Some(path)) => (path.clone(), path.parent().map(Path::to_path_buf)),
        (None, None) => unreachable!("clap requires the screenshot without --paste"),
    };
    let stem = source
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (path, format) = destination(
        &stem,
        beside.as_deref(),
        output.as_deref(),
        outdir.as_deref(),
    )?;
    if path.exists() && !overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", path.display()),
        ));
    }
    let read = picture::read(cli, &source, "conv beautify", 2).map_err(|e| tidied(e, &pasted))?;
    let style = Style {
        background: bg.clone().unwrap_or_else(|| Style::default().background),
        padding: *padding,
        radius: *radius,
        shadow: if *no_shadow {
            Some(false)
        } else if *shadow {
            Some(true)
        } else {
            None
        },
    };
    let plan = beautify::plan(&read.picture, &style)?;
    let mut notes = read.notes;
    if plan.own_shape && style.radius.is_none() && style.shadow.is_none() {
        notes.push(
            "its corners are already clear, so it keeps its own shape and shadow; --radius and --shadow add them anyway"
                .to_string(),
        );
    }
    if *dry_run {
        print_plan(cli, &path, &plan, &notes);
        return Ok(());
    }
    let png = beautify::render(&read.picture, &plan)?;
    output::ensure_dir(outdir.as_deref())?;
    let mut outcome = match format {
        Format::Png => output::write_file(&path, &png)?,
        _ => output::convert_png(cli, &png, &path, format, *overwrite)?,
    };
    notes.append(&mut outcome.warnings);
    outcome.warnings = notes;
    outcome.elapsed_ms = start.elapsed().as_millis() as u64;
    let copied = copy.then(|| convert::copy_outputs(std::slice::from_ref(&outcome.output)));
    print(cli, &outcome, &plan, copied.as_ref());
    Ok(())
}

/// The file to write and its format: OUT as typed, NAME-beautified.EXT for
/// a bare `.ext`, or NAME-beautified.png, beside the input (`beside`) or
/// here, and inside `-o`'s directory when one is given.
fn destination(
    stem: &str,
    beside: Option<&Path>,
    output: Option<&Path>,
    outdir: Option<&Path>,
) -> Result<(PathBuf, Format), ConvError> {
    let named = |ext: &str| {
        let name = PathBuf::from(format!("{stem}{SUFFIX}{ext}"));
        match (outdir, beside) {
            (None, Some(dir)) => dir.join(name),
            _ => name,
        }
    };
    let name = match output {
        None => named(".png"),
        Some(p) if p.is_dir() => {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{} is a directory; to write into it, pass -o {}",
                    p.display(),
                    p.display()
                ),
            ))
        }
        Some(p) if input::is_extension_shorthand(p) => named(&p.to_string_lossy()),
        Some(p) => p.to_path_buf(),
    };
    let path = input::in_outdir(&name, outdir)?;
    let format = input::output_format_of(&path)?;
    let drawable = format == Format::Png
        || (format.kind() == Kind::Image
            && format != Format::Svg
            && registry::lookup(Format::Png, format).is_some());
    if !drawable {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "a beautified screenshot cannot be written as .{}; png, jpg, webp and avif all work",
                format.ext()
            ),
        ));
    }
    Ok((path, format))
}

/// An error about a pasted picture, naming it as such rather than by the
/// folder it passed through.
fn tidied(mut e: ConvError, pasted: &Option<paste::Pasted>) -> ConvError {
    if let Some(p) = pasted.as_ref().filter(|p| p.homeless()) {
        // macOS spells the temporary folder both /var and /private/var.
        let real = std::fs::canonicalize(&p.path).unwrap_or_else(|_| p.path.clone());
        for spelling in [real, p.path.clone()] {
            e.message = e
                .message
                .replace(&spelling.display().to_string(), paste::PASTED);
        }
    }
    e
}

fn plan_json(plan: &Plan) -> serde_json::Value {
    json!({
        "width": plan.width,
        "height": plan.height,
        "scale": plan.scale,
        "background": plan.background.describe(),
        "padding": plan.padding,
        "radius": plan.radius,
        "shadow": plan.shadow,
        "own_shape": plan.own_shape,
    })
}

/// What `--dry-run` would write.
fn print_plan(cli: &Cli, path: &Path, plan: &Plan, notes: &[String]) {
    if cli.json {
        let v = json!({
            "ok": true,
            "dry_run": true,
            "output": path.to_string_lossy(),
            "beautify": plan_json(plan),
            "notes": notes,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return;
    }
    let path = render::safe_for_terminal(&path.display().to_string());
    println!("would write {path}: {}", summary(plan));
    eprint!("{}", render::notes_human(notes, render::stderr_styled()));
}

/// `3104x2024 at 2x on aurora, 112 px padding, 26 px corners, shadow`.
fn summary(plan: &Plan) -> String {
    let mut s = format!("{}x{}", plan.width, plan.height);
    if plan.scale > 1 {
        s.push_str(&format!(" at {}x", plan.scale));
    }
    let bg = match &plan.background {
        Background::Transparent => "a transparent background".to_string(),
        other => other.describe(),
    };
    s.push_str(&format!(", on {bg}, {} px padding", plan.padding));
    if plan.radius > 0 {
        s.push_str(&format!(", {} px corners", plan.radius));
    }
    if plan.shadow {
        s.push_str(", shadow");
    }
    s
}

fn print(cli: &Cli, outcome: &Outcome, plan: &Plan, copied: Option<&convert::CopyReport>) {
    if cli.json {
        let mut v = render::outcome_json(outcome);
        v["beautify"] = plan_json(plan);
        let mut envelope = json!({ "ok": true, "results": [v] });
        if let Some(c) = copied {
            envelope["clipboard"] = c.json();
        }
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return;
    }
    if !cli.quiet {
        print!(
            "{}",
            render::conversion_success_human(outcome, render::stdout_styled())
        );
    }
    eprint!(
        "{}",
        render::conversion_notes_human("", outcome, &[], render::stderr_styled())
    );
    if let Some(c) = copied {
        c.print(cli);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(
        stem: &str,
        beside: Option<&str>,
        output: Option<&str>,
        outdir: Option<&str>,
    ) -> Result<(PathBuf, Format), ConvError> {
        destination(
            stem,
            beside.map(Path::new),
            output.map(Path::new),
            outdir.map(Path::new),
        )
    }

    #[test]
    fn without_out_it_goes_beside_the_input() {
        assert_eq!(
            dest("shot", Some("pics"), None, None).unwrap(),
            (Path::new("pics").join("shot-beautified.png"), Format::Png)
        );
        // A pasted picture has nowhere of its own: here.
        assert_eq!(
            dest("clipboard", None, None, None).unwrap().0,
            PathBuf::from("clipboard-beautified.png")
        );
    }

    #[test]
    fn a_bare_extension_names_the_format() {
        assert_eq!(
            dest("shot", Some(""), Some(".webp"), None).unwrap(),
            (PathBuf::from("shot-beautified.webp"), Format::Webp)
        );
    }

    #[test]
    fn out_is_the_file_as_typed_and_dash_o_the_folder() {
        assert_eq!(
            dest("shot", Some("pics"), Some("post.png"), None)
                .unwrap()
                .0,
            PathBuf::from("post.png")
        );
        assert_eq!(
            dest("shot", Some("pics"), None, Some("out")).unwrap().0,
            Path::new("out").join("shot-beautified.png")
        );
        assert_eq!(
            dest("shot", Some("pics"), Some("post.jpg"), Some("out")).unwrap(),
            (Path::new("out").join("post.jpg"), Format::Jpg)
        );
    }

    #[test]
    fn formats_that_are_not_pictures_are_refused() {
        for out in ["post.svg", "post.mp3", "post.pdf"] {
            let e = dest("shot", None, Some(out), None).unwrap_err();
            assert!(
                matches!(
                    e.code,
                    ErrorCode::UnsupportedPair | ErrorCode::UnknownFormat
                ),
                "{out}: {e:?}"
            );
        }
    }
}
