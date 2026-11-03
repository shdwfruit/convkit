//! `conv qr TEXT [OUT] [-o DIR]`: a QR code, drawn by convkit itself.
//!
//! The paths work as in a conversion: OUT is the file to write, or a bare
//! `.ext` for `qr.EXT`, and `-o` is the directory to write into. PNG and
//! SVG come straight from `convkit_core::qr`; any other format is that PNG
//! sent through the ordinary png -> format conversion.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::qr::{self, Logo};
use convkit_core::{registry, ConvError, ErrorCode, Format, Outcome};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::clipboard::Got;
use crate::commands::{convert, wifi};
use crate::{input, output, paste, render};

pub fn run(cli: &Cli) -> i32 {
    let Some(Command::Qr {
        text,
        output,
        outdir,
        logo,
        size,
        overwrite,
        copy,
        paste,
        wifi,
    }) = &cli.command
    else {
        unreachable!("qr::run is only called for `conv qr`");
    };
    if *wifi {
        return join_code(
            cli,
            text.as_deref(),
            output.as_deref(),
            outdir.as_deref(),
            logo.as_deref(),
            *size,
            *overwrite,
            *copy,
        );
    }
    let pasted = match paste.then(|| pasted(text.as_deref(), output.as_deref(), logo.as_deref())) {
        Some(Ok(p)) => Some(p),
        Some(Err(e)) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
        None => None,
    };
    let (text, output, logo) = match &pasted {
        Some(Pasted::Text(t)) => (t.as_str(), text.as_deref().map(Path::new), logo.as_deref()),
        Some(Pasted::Logo(p)) => (
            text.as_deref().unwrap_or_default(),
            output.as_deref(),
            Some(p.path.as_path()),
        ),
        None => (
            text.as_deref().expect("clap requires TEXT without --paste"),
            output.as_deref(),
            logo.as_deref(),
        ),
    };
    let start = Instant::now();
    let made = output_path(output, outdir.as_deref(), "qr").and_then(|(path, format)| {
        write(
            cli,
            text,
            &path,
            format,
            outdir.as_deref(),
            logo,
            size.unwrap_or(qr::DEFAULT_SIZE),
            *overwrite,
        )
    });
    match made {
        Ok((mut outcome, qr)) => {
            outcome.elapsed_ms = start.elapsed().as_millis() as u64;
            let copied = copy.then(|| convert::copy_outputs(std::slice::from_ref(&outcome.output)));
            print(cli, &outcome, Shown::Text(text), &qr, copied.as_ref());
            0
        }
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

/// `conv qr --wifi [NAME]`: a code that joins a saved network, as `conv
/// wifi NAME --qr` makes, with conv qr's own --size and --logo.
#[allow(clippy::too_many_arguments)]
fn join_code(
    cli: &Cli,
    name: Option<&str>,
    output: Option<&Path>,
    outdir: Option<&Path>,
    logo: Option<&Path>,
    size: Option<u32>,
    overwrite: bool,
    copy: bool,
) -> i32 {
    let start = Instant::now();
    let made = wifi::network(cli, name, |name| {
        let (path, format) = output_path(output, outdir, &wifi::qr_stem(name))?;
        if path.exists() && !overwrite {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} exists; pass -y to overwrite", path.display()),
            ));
        }
        Ok((path, format))
    })
    .and_then(|(network, (path, format))| {
        let text = network.join_text()?;
        let (outcome, qr) = write(
            cli,
            &text,
            &path,
            format,
            outdir,
            logo,
            size.unwrap_or(qr::DEFAULT_SIZE),
            overwrite,
        )?;
        Ok((network, outcome, qr))
    });
    match made {
        Ok((network, mut outcome, qr)) => {
            outcome.elapsed_ms = start.elapsed().as_millis() as u64;
            let mut notes = network.notes.clone();
            notes.append(&mut outcome.warnings);
            outcome.warnings = notes;
            let copied = copy.then(|| convert::copy_outputs(std::slice::from_ref(&outcome.output)));
            print(cli, &outcome, Shown::Wifi(&network), &qr, copied.as_ref());
            0
        }
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

/// What a code holds, as `--json` shows it: the text, or for a Wi-Fi code
/// the network, never its password.
pub(crate) enum Shown<'a> {
    Text(&'a str),
    Wifi(&'a convkit_core::wifi::Network),
}

/// What `--paste` found for the code.
enum Pasted {
    /// Copied text: what the code holds.
    Text(String),
    /// A copied picture or file: the logo.
    Logo(paste::Pasted),
}

/// Reads the clipboard for `conv qr --paste`, given what was typed: TEXT
/// (which is OUT when the clipboard holds the text), OUT and `--logo`.
fn pasted(
    text: Option<&str>,
    output: Option<&Path>,
    logo: Option<&Path>,
) -> Result<Pasted, ConvError> {
    let refused = |m: &str| ConvError::new(ErrorCode::InvalidInvocation, m.to_string());
    match paste::get()? {
        got @ (Got::Files(_) | Got::Picture { .. }) => {
            if logo.is_some() {
                return Err(refused(
                    "the clipboard holds a picture, and --logo names one too",
                ));
            }
            if text.is_none() {
                return Err(refused(
                    "the clipboard holds a picture for the logo; say what the code holds",
                ));
            }
            Ok(Pasted::Logo(paste::from(got, "--logo")?))
        }
        Got::Text(t) => {
            if text.is_some() && output.is_some() {
                return Err(refused(
                    "the clipboard holds the code's text; give at most the file to write",
                ));
            }
            let t = t.trim();
            if t.is_empty() {
                return Err(refused("the clipboard is empty"));
            }
            Ok(Pasted::Text(t.to_string()))
        }
    }
}

/// The file to write and its format: OUT as typed, `STEM.EXT` for a bare
/// `.ext`, or `STEM.png` (`qr.png`), inside `-o`'s directory when one is
/// given.
pub(crate) fn output_path(
    output: Option<&Path>,
    outdir: Option<&Path>,
    stem: &str,
) -> Result<(PathBuf, Format), ConvError> {
    let name = match output {
        None => PathBuf::from(format!("{stem}.png")),
        Some(p) if p.is_dir() => {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{} is a directory; to write {stem}.png into it, pass -o {}",
                    p.display(),
                    p.display()
                ),
            ))
        }
        Some(p) if input::is_extension_shorthand(p) => {
            PathBuf::from(format!("{stem}{}", p.display()))
        }
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
pub(crate) fn write(
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
    output::ensure_dir(outdir)?;
    let mut outcome = match format {
        Format::Svg => output::write_file(path, code.svg.as_bytes())?,
        Format::Png => output::write_file(path, &code.png()?)?,
        _ => output::convert_png(cli, &code.png()?, path, format, overwrite)?,
    };
    let mut warnings = code.warnings(format);
    warnings.append(&mut outcome.warnings);
    outcome.warnings = warnings;
    Ok((outcome, code))
}

pub(crate) fn print(
    cli: &Cli,
    outcome: &Outcome,
    shown: Shown,
    code: &qr::Qr,
    copied: Option<&convert::CopyReport>,
) {
    if cli.json {
        let mut v = render::outcome_json(outcome);
        v["qr"] = json!({
            "size": code.size,
            "version": code.version,
            "error_correction": code.error_correction,
        });
        match shown {
            Shown::Text(text) => v["qr"]["text"] = json!(text),
            Shown::Wifi(network) => v["wifi"] = wifi::network_json(network, false),
        }
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

    fn resolved(
        output: Option<&str>,
        outdir: Option<&str>,
    ) -> Result<(PathBuf, Format), ConvError> {
        output_path(output.map(Path::new), outdir.map(Path::new), "qr")
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
        let e = output_path(Some(dir.path()), None, "qr").unwrap_err();
        assert!(e.message.contains("pass -o"), "{}", e.message);
    }

    #[test]
    fn a_format_that_is_not_a_picture_is_refused() {
        let e = resolved(Some("code.mp3"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert!(e.message.contains(".mp3"), "{}", e.message);
    }
}
