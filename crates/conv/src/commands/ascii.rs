//! `conv ascii PICTURE [OUT] [-o DIR]`: a picture as ASCII art, printed
//! here or written as text, a web page or a picture.
//!
//! The paths work as in `conv qr`: OUT is the file to write, or a bare
//! `.ext` for NAME-ascii.EXT, and `-o` is the directory to write into.
//! Without either, the art is printed, sized to the window when it goes
//! to a terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::ascii::{self, png::drawable, Art, Bg, Fit, Image, Options, Style};
use convkit_core::{frames, registry, Backend, ConvError, ErrorCode, Format, Kind, Outcome};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::{ansi, clipboard, input, install_prompt, output, render};

// Used by the session `conv ascii -i` runs, wired in with the flag.
#[allow(dead_code)]
mod frame;
#[allow(dead_code)]
mod tune;

/// Where the art ends up, which decides its size, its cell shape and
/// whether it can be in colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    /// Printed to a terminal.
    Terminal,
    /// Printed into a pipe or a redirected file.
    Pipe,
    Txt,
    Png,
    Html,
    /// Drawn as a PNG, then converted to this format.
    Image(Format),
}

const TXT_NOTE: &str =
    "a .txt can't hold colour, so it's plain; .png, .html and the terminal keep it";
const JSON_NOTE: &str = "--json gives plain text; write a .png or .html to keep colour";

struct Args<'a> {
    input: &'a Path,
    output: Option<&'a Path>,
    outdir: Option<&'a Path>,
    width: Option<u32>,
    height: Option<u32>,
    chars: &'a str,
    bg: Bg,
    color: bool,
    no_color: bool,
    contrast: i32,
    brightness: i32,
    clipboard: bool,
    overwrite: bool,
}

pub fn run(cli: &Cli) -> i32 {
    let Some(Command::Ascii {
        input,
        output,
        outdir,
        width,
        height,
        chars,
        bg,
        color,
        no_color,
        contrast,
        brightness,
        clipboard,
        overwrite,
    }) = &cli.command
    else {
        unreachable!("main dispatches only Command::Ascii here");
    };
    let args = Args {
        input,
        output: output.as_deref(),
        outdir: outdir.as_deref(),
        width: *width,
        height: *height,
        chars,
        bg: if bg == "light" { Bg::Light } else { Bg::Dark },
        color: *color,
        no_color: *no_color,
        contrast: *contrast,
        brightness: *brightness,
        clipboard: *clipboard,
        overwrite: *overwrite,
    };
    match draw(cli, &args) {
        Ok(()) => 0,
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

fn draw(cli: &Cli, args: &Args) -> Result<(), ConvError> {
    let start = Instant::now();
    let style = Style::parse(args.chars)?;
    let format = source(args.input)?;
    let terminal = std::io::stdout().is_terminal() && !cli.json;
    let (path, shown) = destination(args.input, args.output, args.outdir, terminal)?;
    if let (Shown::Png | Shown::Image(_), Style::Ramp(ramp)) = (shown, &style) {
        if let Some(c) = ramp.iter().find(|&&c| !drawable(c)) {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("a .png can draw ASCII, block and braille characters; \"{c}\" isn't one"),
            ));
        }
    }
    if let Some(path) = &path {
        if path.exists() && !args.overwrite {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} exists; pass -y to overwrite", path.display()),
            ));
        }
    }
    let (img, mut notes) = read_picture(cli, args.input, format)?;
    let img = img.trim_clear();

    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let (mut colour, note) = colour_on(
        &style,
        shown,
        args.color,
        args.no_color,
        no_color_env,
        cli.json,
    );
    notes.extend(note.map(str::to_string));
    if colour && shown == Shown::Terminal && !render::ansi_supported() {
        colour = false;
        notes.push("this console can't show colour".to_string());
    }

    let cell_aspect = cell_aspect(shown);
    let fit = match (args.width, args.height) {
        (Some(w), None) => Fit::Width(w),
        (None, Some(h)) => Fit::Height(h),
        (Some(w), Some(h)) => Fit::Box(w, h),
        (None, None) if shown == Shown::Terminal => {
            let (rows, cols) = console::Term::stdout().size();
            Fit::Box(
                u32::from(cols).max(1),
                u32::from(rows).saturating_sub(1).max(1),
            )
        }
        (None, None) => Fit::Width(ascii::DEFAULT_COLS),
    };
    let (cols, rows, cap) = ascii::layout(img.width, img.height, fit, cell_aspect);
    notes.extend(cap);
    let opts = Options {
        style,
        bg: args.bg,
        colour,
        contrast: args.contrast,
        brightness: args.brightness,
        cell_aspect,
    };
    let made = ascii::make(&img, cols, rows, &opts);
    notes.extend(made.notes);
    let art = made.art;
    if args.clipboard {
        let text = clipboard_art(&img, &art, &opts).text();
        let chars = text.chars().count();
        notes.push(match clipboard::copy(&text) {
            Ok(how) => clipboard::note(chars, discord_width(img.width, img.height, 2.0), &how),
            Err(e) => format!("couldn't copy it: {e}"),
        });
    }

    let Some(path) = path else {
        return print_art(cli, &art, &opts, notes);
    };
    output::ensure_dir(args.outdir)?;
    let title = args.input.file_name().unwrap_or_default().to_string_lossy();
    let mut outcome = match shown {
        Shown::Txt => output::write_file(&path, art.text().as_bytes())?,
        Shown::Html => {
            output::write_file(&path, ascii::html::html(&art, args.bg, &title).as_bytes())?
        }
        Shown::Png => output::write_file(&path, &ascii::png::png(&art, args.bg)?)?,
        Shown::Image(f) => output::convert_png(
            cli,
            &ascii::png::png(&art, args.bg)?,
            &path,
            f,
            args.overwrite,
        )?,
        Shown::Terminal | Shown::Pipe => unreachable!("a path is a file"),
    };
    outcome.elapsed_ms = start.elapsed().as_millis() as u64;
    notes.append(&mut outcome.warnings);
    outcome.warnings = notes;
    print_written(cli, &outcome, &art, &opts);
    Ok(())
}

/// The art printed: plain or in the terminal's colours, notes on stderr,
/// or under `--json` the text with its size.
fn print_art(cli: &Cli, art: &Art, opts: &Options, notes: Vec<String>) -> Result<(), ConvError> {
    if cli.json {
        let v = json!({
            "ok": true,
            "results": [{
                "ok": true,
                "text": art.text(),
                "cols": art.cols,
                "rows": art.rows,
                "style": opts.style.name(),
                "warnings": notes,
            }],
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return Ok(());
    }
    let text = if opts.colour {
        ansi::paint(art, ansi::truecolor(&|k| std::env::var(k).ok()))
    } else {
        art.text()
    };
    // A reader that stops early (`| head`) is not an error.
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    eprint!("{}", render::notes_human(&notes, render::stderr_styled()));
    Ok(())
}

fn print_written(cli: &Cli, outcome: &Outcome, art: &Art, opts: &Options) {
    if cli.json {
        let mut v = render::outcome_json(outcome);
        v["ascii"] = json!({
            "cols": art.cols,
            "rows": art.rows,
            "style": opts.style.name(),
            "colour": opts.colour,
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

/// The art as it goes on the clipboard: plain text, so coloured blocks
/// (which are all `▀` without their colours) are drawn again as shades.
fn clipboard_art(img: &Image, art: &Art, opts: &Options) -> Art {
    if opts.style == Style::Blocks && opts.colour {
        let plain = Options {
            colour: false,
            ..opts.clone()
        };
        return ascii::make(img, art.cols, art.rows, &plain).art;
    }
    art.clone()
}

/// The most columns whose art, inside ``` fences, fits one Discord
/// message of 2,000 characters.
fn discord_width(pic_w: u32, pic_h: u32, cell_aspect: f32) -> u32 {
    let size = |c: u32| {
        let (cols, rows, _) = ascii::layout(pic_w, pic_h, Fit::Width(c), cell_aspect);
        cols * rows + rows + 8
    };
    (1..=ascii::MAX_SIDE_CELLS)
        .take_while(|&c| size(c) <= 2000)
        .last()
        .unwrap_or(1)
}

/// What the input is: `None` for a file whose extension conv doesn't know,
/// which ffmpeg still gets to try.
fn source(input: &Path) -> Result<Option<Format>, ConvError> {
    if input.is_dir() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{} is a folder; conv ascii takes one picture",
                input.display()
            ),
        ));
    }
    if !input.exists() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("no such file: {}", input.display()),
        ));
    }
    match Format::from_path(input) {
        Some(f) if matches!(f.kind(), Kind::Image | Kind::Video) => Ok(Some(f)),
        Some(_) => {
            let name = input.file_name().unwrap_or_default().to_string_lossy();
            let ext = input.extension().unwrap_or_default().to_string_lossy();
            Err(ConvError::new(
                ErrorCode::UnsupportedPair,
                format!("conv ascii draws pictures and videos; {name} is a {ext} file"),
            ))
        }
        None => Ok(None),
    }
}

/// Where the art goes, by the `conv qr` rules for OUT and `-o`.
fn destination(
    input: &Path,
    output: Option<&Path>,
    outdir: Option<&Path>,
    stdout_terminal: bool,
) -> Result<(Option<PathBuf>, Shown), ConvError> {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let name = match output {
        None if outdir.is_none() => {
            let shown = if stdout_terminal {
                Shown::Terminal
            } else {
                Shown::Pipe
            };
            return Ok((None, shown));
        }
        None => PathBuf::from(format!("{stem}-ascii.txt")),
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
        Some(p) if input::is_extension_shorthand(p) => {
            PathBuf::from(format!("{stem}-ascii{}", p.display()))
        }
        Some(p) => p.to_path_buf(),
    };
    let path = input::in_outdir(&name, outdir)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let shown = match ext.as_str() {
        "txt" => Shown::Txt,
        "png" => Shown::Png,
        "html" | "htm" => Shown::Html,
        _ => match Format::from_ext(&ext) {
            Some(f)
                if f != Format::Svg
                    && !Format::is_read_only_ext(&ext)
                    && registry::lookup(Format::Png, f).is_some() =>
            {
                Shown::Image(f)
            }
            _ => {
                let what = if ext.is_empty() {
                    format!("{} has no extension", path.display())
                } else {
                    format!("not .{ext}")
                };
                return Err(ConvError::new(
                    ErrorCode::UnsupportedPair,
                    format!("conv ascii writes .txt, .png, .html, or another picture format like .jpg, .webp or .pdf; {what}"),
                ));
            }
        },
    };
    Ok((Some(path), shown))
}

/// Whether the art keeps the picture's colours: when asked, except where
/// colour can't go (a .txt, `--json`'s text); and by default for blocks,
/// which say little without it, wherever colour can show. `NO_COLOR` only
/// turns off the terminal's colour, never a file's.
fn colour_on(
    style: &Style,
    shown: Shown,
    color: bool,
    no_color: bool,
    no_color_env: bool,
    json: bool,
) -> (bool, Option<&'static str>) {
    if shown == Shown::Txt {
        return (false, color.then_some(TXT_NOTE));
    }
    if json && matches!(shown, Shown::Terminal | Shown::Pipe) {
        return (false, color.then_some(JSON_NOTE));
    }
    if no_color {
        return (false, None);
    }
    if color {
        return (true, None);
    }
    let on = *style == Style::Blocks
        && match shown {
            Shown::Png | Shown::Html | Shown::Image(_) => true,
            Shown::Terminal => !no_color_env,
            Shown::Pipe | Shown::Txt => false,
        };
    (on, None)
}

/// Reads the picture: SVG drawn by conv, HEIC/HEIF through conv's own
/// heic -> png conversion (ffmpeg 6.1 can't open them, and 9.0 reads an
/// iPhone photo's tile grid as one tile without a word), everything else
/// through ffmpeg, falling back to that conversion route if ffmpeg can't
/// read it.
fn read_picture(
    cli: &Cli,
    input: &Path,
    format: Option<Format>,
) -> Result<(Image, Vec<String>), ConvError> {
    if format == Some(Format::Svg) {
        let bytes = std::fs::read(input).map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {}: {e}", input.display()),
            )
        })?;
        let mut notes = Vec::new();
        if ascii::svg_has_text(&bytes) {
            notes.push(
                "the SVG's text is not drawn; convert its text to outlines (paths) to keep it"
                    .to_string(),
            );
        }
        return Ok((ascii::from_svg(&bytes, ascii::READ_MAX_SIDE)?, notes));
    }
    let mut ffmpeg_error = None;
    if !matches!(format, Some(Format::Heic | Format::Heif)) {
        let read = install_prompt::resolve_or_install(cli, Backend::Ffmpeg).and_then(|ffmpeg| {
            let png = frames::still(&ffmpeg, input, ascii::READ_MAX_SIDE)?;
            ascii::decode_png(&png)
        });
        match read {
            Ok(img) => return Ok((img, vec![])),
            // ffmpeg is the one to install; another backend wouldn't help.
            Err(e) if e.code == ErrorCode::BackendMissing => return Err(e),
            Err(e) => ffmpeg_error = Some(e),
        }
    }
    let Some(from) = format.filter(|&f| registry::lookup(f, Format::Png).is_some()) else {
        return Err(ffmpeg_error.unwrap_or_else(|| {
            ConvError::new(
                ErrorCode::UnsupportedPair,
                format!("conv ascii cannot read {}", input.display()),
            )
        }));
    };
    let temp = std::env::temp_dir().join(format!("conv-ascii-{}.png", std::process::id()));
    let read = output::convert(cli, from, input, Format::Png, &temp, true).and_then(|_| {
        let bytes = std::fs::read(&temp).map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {}: {e}", temp.display()),
            )
        })?;
        ascii::decode_png(&bytes)
    });
    let _ = std::fs::remove_file(&temp);
    Ok((read?, vec![]))
}

/// How many times taller than wide a character cell is where the art is
/// shown.
fn cell_aspect(shown: Shown) -> f32 {
    use convkit_core::ascii::png::{CELL_H, CELL_W};
    match shown {
        Shown::Terminal => terminal_cell_aspect().unwrap_or(2.0),
        Shown::Png | Shown::Image(_) => CELL_H as f32 / CELL_W as f32,
        Shown::Pipe | Shown::Txt | Shown::Html => 2.0,
    }
}

/// The terminal's real cell shape, from the pixel size it reports with its
/// window size (Ghostty, kitty, iTerm2 and WezTerm do; others report 0).
/// An ioctl, not a question the terminal answers on stdin.
#[cfg(unix)]
fn terminal_cell_aspect() -> Option<f32> {
    // SAFETY: TIOCGWINSZ fills in a winsize for a terminal; on any other
    // fd it fails and the zeroed struct is ignored.
    let ws = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) != 0 {
            return None;
        }
        ws
    };
    if ws.ws_row == 0 || ws.ws_col == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    let cell_h = f32::from(ws.ws_ypixel) / f32::from(ws.ws_row);
    let cell_w = f32::from(ws.ws_xpixel) / f32::from(ws.ws_col);
    Some((cell_h / cell_w).clamp(1.0, 4.0))
}

#[cfg(not(unix))]
fn terminal_cell_aspect() -> Option<f32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(out: Option<&str>, dir: Option<&str>) -> Result<(Option<PathBuf>, Shown), ConvError> {
        destination(
            Path::new("cat.jpg"),
            out.map(Path::new),
            dir.map(Path::new),
            false,
        )
    }

    #[test]
    fn destination_follows_the_qr_rules() {
        assert_eq!(
            destination(Path::new("cat.jpg"), None, None, true).unwrap(),
            (None, Shown::Terminal)
        );
        assert_eq!(dest(None, None).unwrap(), (None, Shown::Pipe));
        assert_eq!(
            dest(Some(".png"), None).unwrap(),
            (Some(PathBuf::from("cat-ascii.png")), Shown::Png)
        );
        assert_eq!(
            dest(None, Some("out")).unwrap(),
            (Some(Path::new("out").join("cat-ascii.txt")), Shown::Txt)
        );
        assert_eq!(
            dest(Some("art.html"), Some("out")).unwrap(),
            (Some(Path::new("out").join("art.html")), Shown::Html)
        );
        assert!(dest(Some("a/x.txt"), Some("out")).is_err());
        let e = dest(Some("x.svg"), None).unwrap_err();
        assert!(e.message.contains("conv ascii writes"), "{}", e.message);
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert_eq!(
            dest(Some("x.webp"), None).unwrap(),
            (Some(PathBuf::from("x.webp")), Shown::Image(Format::Webp))
        );
    }

    #[test]
    fn source_refuses_non_pictures() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["doc.pdf", "noext", "cat.jpg"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let e = source(&dir.path().join("doc.pdf")).unwrap_err();
        assert!(e.message.contains("draws pictures"), "{}", e.message);
        assert_eq!(
            source(&dir.path().join("missing.jpg")).unwrap_err().code,
            ErrorCode::InputNotFound
        );
        assert_eq!(source(&dir.path().join("noext")).unwrap(), None);
        assert_eq!(
            source(&dir.path().join("cat.jpg")).unwrap(),
            Some(Format::Jpg)
        );
        assert_eq!(
            source(dir.path()).unwrap_err().code,
            ErrorCode::InvalidInvocation
        );
    }

    #[test]
    fn colour_defaults_table() {
        use Shown::*;
        let on =
            |style: Style, shown: Shown, color: bool, no_color: bool, env: bool, json: bool| {
                colour_on(&style, shown, color, no_color, env, json)
            };
        assert_eq!(
            on(Style::Blocks, Terminal, false, false, false, false),
            (true, None)
        );
        assert_eq!(
            on(Style::Blocks, Terminal, false, false, true, false),
            (false, None)
        );
        assert_eq!(
            on(Style::Blocks, Terminal, true, false, true, false),
            (true, None)
        );
        assert_eq!(
            on(Style::Blocks, Pipe, false, false, false, false),
            (false, None)
        );
        assert_eq!(
            on(Style::Blocks, Png, false, false, true, false),
            (true, None)
        );
        assert_eq!(
            on(Style::Blocks, Html, false, false, false, false),
            (true, None)
        );
        assert_eq!(
            on(
                Style::Blocks,
                Image(Format::Jpg),
                false,
                false,
                false,
                false
            ),
            (true, None)
        );
        assert_eq!(
            on(Style::Blocks, Png, false, true, false, false),
            (false, None)
        );
        assert_eq!(
            on(Style::Ascii, Png, false, false, false, false),
            (false, None)
        );
        assert_eq!(
            on(Style::Ascii, Terminal, false, false, false, false),
            (false, None)
        );
        assert_eq!(
            on(Style::Ascii, Pipe, true, false, false, false),
            (true, None)
        );
        let (c, note) = on(Style::Ascii, Txt, true, false, false, false);
        assert!(!c && note.unwrap().contains("can't hold colour"));
        assert_eq!(
            on(Style::Blocks, Txt, false, false, false, false),
            (false, None)
        );
        let (c, note) = on(Style::Blocks, Pipe, true, false, false, true);
        assert!(!c && note.unwrap().contains("--json gives plain text"));
        assert_eq!(
            on(Style::Blocks, Terminal, false, false, false, true),
            (false, None)
        );
    }
    #[test]
    fn discord_width_fits() {
        let c = discord_width(400, 300, 2.0);
        let size = |c: u32| {
            let (cols, rows, _) = ascii::layout(400, 300, Fit::Width(c), 2.0);
            cols * rows + rows + 8
        };
        assert!(size(c) <= 2000, "{c}");
        assert!(size(c + 1) > 2000, "{c}");
    }

    #[test]
    fn clipboard_text_is_plain_for_coloured_blocks() {
        let img = Image {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 255,
            ],
        };
        let opts = Options {
            style: Style::Blocks,
            bg: Bg::Dark,
            colour: true,
            contrast: 0,
            brightness: 0,
            cell_aspect: 2.0,
        };
        let art = ascii::make(&img, 4, 2, &opts).art;
        assert!(art.is_blocks());
        let text = clipboard_art(&img, &art, &opts).text();
        assert!(!text.contains('▀') && !text.contains('▄'), "{text}");
        assert!(text.chars().any(|c| "░▒▓█".contains(c)), "{text}");
        let plain = Options {
            style: Style::Ascii,
            ..opts
        };
        let art = ascii::make(&img, 4, 2, &plain).art;
        assert_eq!(clipboard_art(&img, &art, &plain), art);
    }
}
