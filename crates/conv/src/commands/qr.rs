//! `conv qr TEXT [OUT] [-o DIR]`: a QR code, drawn by convkit itself.
//! `conv qr --read PICTURE...`: the codes in pictures, printed.
//!
//! The paths work as in a conversion: OUT is the file to write, or a bare
//! `.ext` for `qr.EXT`, and `-o` is the directory to write into. PNG and
//! SVG come straight from `convkit_core::qr`; any other format is that PNG
//! sent through the ordinary png -> format conversion.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::qr::read::{self as reader, Code, Picture};
use convkit_core::qr::{self, Logo};
use convkit_core::{registry, ConvError, ErrorCode, Format, Kind, Outcome};
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
        read,
    }) = &cli.command
    else {
        unreachable!("qr::run is only called for `conv qr`");
    };
    if !read.is_empty() {
        return read_all(cli, read);
    }
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
            text.as_deref()
                .expect("clap requires TEXT without --paste, --wifi or --read"),
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
    if let Some(hint) = names_a_picture(text) {
        warnings.push(hint);
    }
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

/// A note for TEXT that is also the name of a picture here: the code holds
/// the name, as asked, and `--read` is how to read the picture instead.
fn names_a_picture(text: &str) -> Option<String> {
    let path = Path::new(text);
    let picture = Format::from_path(path).is_some_and(|f| f.kind() == Kind::Image);
    (picture && path.is_file()).then(|| {
        let command =
            render::command_line_human("conv", &["qr".into(), "--read".into(), text.into()]);
        format!(
            "the code holds the text \"{text}\"; to read the code in the picture {text} instead, run: {command}"
        )
    })
}

/// `conv qr --read`: reads every picture, then prints what was found. The
/// exit code is the batch rule: 0 when every picture had a code, the
/// failure's own code when none did, 4 for a mix.
fn read_all(cli: &Cli, paths: &[PathBuf]) -> i32 {
    let paths = input::expand_globs(paths, paths.len());
    let results: Vec<(PathBuf, Result<Vec<Code>, ConvError>)> = paths
        .iter()
        .map(|p| (p.clone(), read_codes(cli, p)))
        .collect();
    if cli.json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|(p, r)| match r {
                Ok(codes) => json!({ "ok": true, "input": p.to_string_lossy(), "codes": codes }),
                Err(e) => json!({ "ok": false, "input": p.to_string_lossy(), "error": e }),
            })
            .collect();
        let ok = results.iter().all(|(_, r)| r.is_ok());
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": ok, "results": arr })).unwrap()
        );
    } else {
        print_read(cli, &results);
    }
    let failures: Vec<&ConvError> = results
        .iter()
        .filter_map(|(_, r)| r.as_ref().err())
        .collect();
    match failures.len() {
        0 => 0,
        n if n == results.len() => failures[0].code.exit_code(),
        _ => ErrorCode::BatchPartialFailure.exit_code(),
    }
}

/// Each code's text on its own line of stdout, after `NAME: ` when there
/// are several pictures, so the output pipes. A Wi-Fi code's network goes
/// to stderr under it, and so does a picture without a code.
fn print_read(cli: &Cli, results: &[(PathBuf, Result<Vec<Code>, ConvError>)]) {
    let tty = std::io::stdout().is_terminal();
    let labelled = results.len() > 1;
    let mut out = std::io::stdout().lock();
    for (path, result) in results {
        let label = if labelled {
            format!("{}: ", path.display())
        } else {
            String::new()
        };
        match result {
            Ok(codes) => {
                for code in codes {
                    let mut line = format!("{label}{}", code.text);
                    if tty {
                        line = render::safe_for_terminal(&line);
                    }
                    // A closed pipe (`| head -1`) is not worth a panic.
                    if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                        return;
                    }
                    if let (Some(w), false) = (&code.wifi, cli.quiet) {
                        eprint!("{}", wifi_human(w));
                    }
                }
            }
            Err(e) => {
                let mut msg = render::error_human(e);
                if labelled && !e.message.contains(&path.display().to_string()) {
                    msg = msg.replacen("error: ", &format!("error: {label}"), 1);
                }
                eprint!("{}", render::safe_for_terminal(&msg));
            }
        }
    }
}

/// A Wi-Fi code's network, indented, for stderr.
fn wifi_human(w: &reader::Wifi) -> String {
    let security = if w.security.eq_ignore_ascii_case("nopass") {
        "none (an open network)".to_string()
    } else {
        w.security.clone()
    };
    let mut s = format!("  Wi-Fi network: {}\n  security: {security}\n", w.network);
    if let Some(p) = &w.password {
        s.push_str(&format!("  password: {p}\n"));
    }
    if w.hidden {
        s.push_str("  hidden: yes, so join it by name\n");
    }
    render::safe_for_terminal(&s)
}

/// The codes in one picture. conv decodes png, jpg, gif, webp and svg
/// itself; any other picture that converts to PNG (heic, avif, tiff, ...)
/// goes through that conversion first, as does a file conv's own decoder
/// cannot read (a CMYK JPEG, say).
fn read_codes(cli: &Cli, path: &Path) -> Result<Vec<Code>, ConvError> {
    let name = path.display();
    if path.is_dir() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} is a folder; --read takes pictures"),
        ));
    }
    if !path.exists() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("no such file: {name}"),
        ));
    }
    let format = Format::from_path(path);
    if format == Some(Format::Pdf) {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!("--read takes pictures, and cannot read a PDF's pages yet; take a screenshot of the page with the code and read that instead of {name}"),
        ));
    }
    let format = format.filter(|f| f.kind() == Kind::Image).ok_or_else(|| {
        ConvError::new(
            ErrorCode::UnsupportedPair,
            format!("--read takes pictures (png, jpg, heic, webp, ...); {name} is not one"),
        )
    })?;
    let converts = format != Format::Png && registry::lookup(format, Format::Png).is_some();
    let mut pictures = Vec::new();
    if reader::decodes_itself(format) {
        let bytes = std::fs::read(path).map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {name}: {e}"),
            )
        })?;
        match reader::decode(&bytes, format) {
            Ok(p) => pictures.push(p),
            Err(e) if !converts || format == Format::Svg => return Err(e),
            Err(_) => {}
        }
    }
    if pictures.is_empty() {
        if !converts {
            return Err(ConvError::new(
                ErrorCode::UnsupportedPair,
                format!("conv cannot read a .{} picture", format.ext()),
            ));
        }
        pictures = converted(cli, format, path)?;
    }
    let mut codes: Vec<Code> = Vec::new();
    for pic in &pictures {
        for code in reader::read(pic) {
            if !codes.iter().any(|c| c.text == code.text) {
                codes.push(code);
            }
        }
    }
    if codes.is_empty() {
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            "no QR code found; if there is one, try a sharper picture with the whole code in it, closer and straighter",
        ));
    }
    Ok(codes)
}

/// The picture converted to PNG in a private temporary folder and read
/// back: every PNG the conversion leaves there, in case it writes one a
/// frame or layer.
fn converted(cli: &Cli, format: Format, path: &Path) -> Result<Vec<Picture>, ConvError> {
    let (dir, temp) = output::private_temp("picture.png")?;
    output::convert(cli, format, path, Format::Png, &temp, true)?;
    let mut pngs: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {}: {e}", dir.path().display()),
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    pngs.sort_by(|a, b| input::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    pngs.iter()
        .map(|p| {
            let bytes = std::fs::read(p).map_err(|e| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("cannot read {}: {e}", p.display()),
                )
            })?;
            reader::decode(&bytes, Format::Png)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_shown_not_sent_to_the_terminal() {
        assert_eq!(
            render::safe_for_terminal("a\u{1b}[31mb\tc\nd\u{202e}"),
            "a\\u{1b}[31mb\tc\nd\\u{202e}"
        );
    }

    #[test]
    fn a_wifi_code_cannot_reach_the_terminal() {
        let w =
            reader::parse_wifi("WIFI:S:a\u{1b}]52\\;c\\;aGk=\u{7};T:WPA;P:p\u{1b}[2J;;").unwrap();
        let s = wifi_human(&w);
        assert!(!s.contains('\u{1b}') && !s.contains('\u{7}'), "{s:?}");
        assert!(s.contains(r"a\u{1b}]52;c;aGk=\u{7}"), "{s}");
    }

    #[test]
    fn wifi_is_shown_parsed() {
        let w = reader::parse_wifi("WIFI:S:Home;T:WPA;P:secret;;").unwrap();
        assert_eq!(
            wifi_human(&w),
            "  Wi-Fi network: Home\n  security: WPA\n  password: secret\n"
        );
        let open = reader::parse_wifi("WIFI:S:Cafe;T:nopass;H:true;;").unwrap();
        assert_eq!(
            wifi_human(&open),
            "  Wi-Fi network: Cafe\n  security: none (an open network)\n  hidden: yes, so join it by name\n"
        );
    }

    #[test]
    fn text_naming_a_picture_here_gets_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("menu.png");
        std::fs::write(&pic, b"x").unwrap();
        let text = pic.to_str().unwrap();
        let hint = names_a_picture(text).unwrap();
        // Quoted as a shell needs it: a Windows path is.
        let command =
            render::command_line_human("conv", &["qr".into(), "--read".into(), text.into()]);
        assert!(hint.ends_with(&format!("run: {command}")), "{hint}");
        // Not a file here, or not a picture: no hint.
        assert_eq!(names_a_picture("menu.png-that-is-not-here.png"), None);
        let doc = dir.path().join("notes.txt");
        std::fs::write(&doc, b"x").unwrap();
        assert_eq!(names_a_picture(doc.to_str().unwrap()), None);
    }

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
