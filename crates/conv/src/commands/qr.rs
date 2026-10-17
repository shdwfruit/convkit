//! `conv qr TEXT [OUT] [-o DIR]`: a QR code, drawn by convkit itself.
//!
//! The paths work as in a conversion: OUT is the file to write, or a bare
//! `.ext` for `qr.EXT`, and `-o` is the directory to write into. PNG and
//! SVG come straight from `convkit_core::qr`; any other format is that PNG
//! sent through the ordinary png -> format conversion.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::qr::{self, Logo};
use convkit_core::{exec, registry, ConvError, ErrorCode, Format, Outcome, Request, Tuning};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::commands::install;
use crate::{input, install_prompt, render};

pub fn run(cli: &Cli) -> i32 {
    let Some(Command::Qr {
        text,
        output,
        outdir,
        logo,
        size,
        overwrite,
    }) = &cli.command
    else {
        unreachable!("qr::run is only called for `conv qr`");
    };
    let start = Instant::now();
    let made = output_path(output.as_deref(), outdir.as_deref()).and_then(|(path, format)| {
        write(
            cli,
            text,
            &path,
            format,
            outdir.as_deref(),
            logo.as_deref(),
            size.unwrap_or(qr::DEFAULT_SIZE),
            *overwrite,
        )
    });
    match made {
        Ok((mut outcome, qr)) => {
            outcome.elapsed_ms = start.elapsed().as_millis() as u64;
            print(cli, &outcome, text, &qr);
            0
        }
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

/// The file to write and its format: OUT as typed, `qr.EXT` for a bare
/// `.ext`, or `qr.png`, inside `-o`'s directory when one is given.
fn output_path(
    output: Option<&Path>,
    outdir: Option<&Path>,
) -> Result<(PathBuf, Format), ConvError> {
    let name = match output {
        None => PathBuf::from("qr.png"),
        Some(p) if p.is_dir() => {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{} is a directory; to write qr.png into it, pass -o {}",
                    p.display(),
                    p.display()
                ),
            ))
        }
        Some(p) if input::is_extension_shorthand(p) => PathBuf::from(format!("qr{}", p.display())),
        Some(p) => p.to_path_buf(),
    };
    let path = input::in_outdir(&name, outdir)?;
    let format = input::output_format_of(&path)?;
    if !matches!(format, Format::Png | Format::Svg)
        && registry::lookup(Format::Png, format).is_none()
    {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "a QR code cannot be written as .{}; png, svg, jpg, webp and pdf all work",
                format.ext()
            ),
        ));
    }
    Ok((path, format))
}

#[allow(clippy::too_many_arguments)]
fn write(
    cli: &Cli,
    text: &str,
    path: &Path,
    format: Format,
    outdir: Option<&Path>,
    logo: Option<&Path>,
    size: u32,
    overwrite: bool,
) -> Result<(Outcome, qr::Qr), ConvError> {
    if path.exists() && !overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", path.display()),
        ));
    }
    let logo = logo.map(Logo::read).transpose()?;
    let code = qr::make(text, logo.as_ref(), size)?;
    if let Some(dir) = outdir {
        std::fs::create_dir_all(dir).map_err(|e| {
            ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("cannot create output directory {}: {e}", dir.display()),
            )
        })?;
    }
    let mut outcome = match format {
        Format::Svg => write_file(path, code.svg.as_bytes())?,
        Format::Png => write_file(path, &code.png()?)?,
        _ => convert_png(cli, &code.png()?, path, format, overwrite)?,
    };
    let mut warnings = code.warnings(format);
    warnings.append(&mut outcome.warnings);
    outcome.warnings = warnings;
    Ok((outcome, code))
}

/// Writes `bytes` beside `path` and renames it into place, so a failed
/// write never leaves half a file under the name asked for.
fn write_file(path: &Path, bytes: &[u8]) -> Result<Outcome, ConvError> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.conv-{}", std::process::id()));
    let failed = |e: std::io::Error| {
        let _ = std::fs::remove_file(&temp);
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", path.display()),
        )
    };
    std::fs::write(&temp, bytes).map_err(failed)?;
    std::fs::rename(&temp, path).map_err(failed)?;
    Ok(Outcome {
        output: path.to_path_buf(),
        bytes: bytes.len() as u64,
        warnings: vec![],
        notes: vec![],
        backend_output: vec![],
        backends: vec![],
        remuxed: false,
        elapsed_ms: 0,
        sizing: None,
        enlarged: None,
        range: None,
        speed: None,
    })
}

/// Converts the drawn PNG to `format` as `conv qr.png out.FORMAT` would,
/// offering to install a missing backend the same way.
fn convert_png(
    cli: &Cli,
    png: &[u8],
    path: &Path,
    format: Format,
    overwrite: bool,
) -> Result<Outcome, ConvError> {
    let temp = std::env::temp_dir().join(format!("conv-qr-{}.png", std::process::id()));
    std::fs::write(&temp, png).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", temp.display()),
        )
    })?;
    let req = Request {
        from: Format::Png,
        to: format,
        inputs: vec![temp.clone()],
        output: path.to_path_buf(),
        overwrite,
        tuning: Tuning::default(),
        allow_extreme: false,
    };
    let mut result = exec::run(&req, &cli.resolver(), &mut |_| {});
    if let Err(e) = &result {
        if let (ErrorCode::BackendMissing, Some(backend)) = (e.code, e.backend) {
            if install_prompt::should_install(cli, backend)
                && install::install_backend(cli, backend).is_ok()
            {
                result = exec::run(&req, &cli.resolver(), &mut |_| {});
            }
        }
    }
    let _ = std::fs::remove_file(&temp);
    result
}

fn print(cli: &Cli, outcome: &Outcome, text: &str, code: &qr::Qr) {
    if cli.json {
        let mut v = render::outcome_json(outcome);
        v["qr"] = json!({
            "text": text,
            "size": code.size,
            "version": code.version,
            "error_correction": code.error_correction,
        });
        let envelope = json!({ "ok": true, "results": [v] });
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(
        output: Option<&str>,
        outdir: Option<&str>,
    ) -> Result<(PathBuf, Format), ConvError> {
        output_path(output.map(Path::new), outdir.map(Path::new))
    }

    #[test]
    fn without_out_it_writes_qr_png_here() {
        assert_eq!(
            resolved(None, None).unwrap(),
            (PathBuf::from("qr.png"), Format::Png)
        );
    }

    #[test]
    fn a_bare_extension_names_the_format() {
        assert_eq!(
            resolved(Some(".svg"), None).unwrap(),
            (PathBuf::from("qr.svg"), Format::Svg)
        );
    }

    #[test]
    fn out_is_the_file_as_typed() {
        assert_eq!(
            resolved(Some("menu/table4.jpg"), None).unwrap(),
            (PathBuf::from("menu/table4.jpg"), Format::Jpg)
        );
    }

    #[test]
    fn dash_o_is_the_directory_to_write_into() {
        assert_eq!(
            resolved(None, Some("out")).unwrap().0,
            Path::new("out").join("qr.png")
        );
        assert_eq!(
            resolved(Some(".svg"), Some("out")).unwrap().0,
            Path::new("out").join("qr.svg")
        );
        assert_eq!(
            resolved(Some("code.webp"), Some("out")).unwrap().0,
            Path::new("out").join("code.webp")
        );
    }

    #[test]
    fn a_directory_in_both_out_and_dash_o_is_refused() {
        let e = resolved(Some("a/code.png"), Some("out")).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }

    #[test]
    fn an_existing_directory_as_out_points_at_dash_o() {
        let dir = tempfile::tempdir().unwrap();
        let e = output_path(Some(dir.path()), None).unwrap_err();
        assert!(e.message.contains("pass -o"), "{}", e.message);
    }

    #[test]
    fn a_format_that_is_not_a_picture_is_refused() {
        let e = resolved(Some("code.mp3"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert!(e.message.contains(".mp3"), "{}", e.message);
    }
}
