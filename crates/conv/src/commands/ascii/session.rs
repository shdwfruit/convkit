//! The session `conv ascii -i` runs: the picture read once, the art
//! remade and drawn as keys change it, written or copied on request, and
//! the command for it printed at the end.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use console::Key;
use convkit_core::ascii::{self, Art, Fit, Image, Options, Style};
use convkit_core::{registry, ConvError, ErrorCode, Format, Outcome};

use super::frame::{self, max_width};
use super::tune::{act, command_line, command_output, Invocation, Next, Tune, LEAST_WIDTH};
use super::{
    cell_aspect, check_drawable, clipboard_art, colour_on, destination, discord_width,
    read_picture, write_art, Args, Shown,
};
use crate::cli::Cli;
use crate::screen::{self, Screen};
use crate::{ansi, clipboard, install_prompt, output, render};

/// How long the session waits for a key before looking at the window's
/// size again.
const RESIZE_CHECK: Duration = Duration::from_millis(150);

/// Refuses what can't work before the screen is touched, reads the
/// picture, then runs the session.
pub(super) fn run(
    cli: &Cli,
    args: &Args,
    style: Style,
    format: Option<Format>,
) -> Result<(), ConvError> {
    if cli.json || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "conv ascii -i needs a terminal; leave out -i to print or write the art",
        ));
    }
    // OUT, when given, is where `w` writes; its backend is found (or
    // offered for install) now, not halfway through the session.
    let out = match args.output {
        Some(_) => {
            let (path, shown) = destination(args.input, args.output, args.outdir, false)?;
            Some((path.expect("OUT names a file"), shown))
        }
        None => None,
    };
    if let Some((_, Shown::Image(f))) = &out {
        for backend in registry::backends_for(Format::Png, *f) {
            install_prompt::resolve_or_install(cli, backend)?;
        }
    }
    let (img, notes) = read_picture(cli, args.input, format)?;
    session(cli, args, style, img.trim_clear(), notes, out)
}

/// What the art was last made from, so it is only made again on a change.
#[derive(PartialEq)]
struct Made {
    cols: u32,
    rows: u32,
    opts: Options,
}

fn session(
    cli: &Cli,
    args: &Args,
    style: Style,
    img: Image,
    notes: Vec<String>,
    out: Option<(PathBuf, Shown)>,
) -> Result<(), ConvError> {
    let env = |k: &str| std::env::var(k).ok();
    let truecolor = ansi::truecolor(&env);
    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let out_shown = out.as_ref().map_or(Shown::Terminal, |(_, s)| *s);
    let colour = colour_on(
        &style,
        out_shown,
        args.color,
        args.no_color,
        no_color_env,
        false,
    )
    .0;
    let aspect = cell_aspect(Shown::Terminal);
    let pic = (img.width, img.height);
    let fits = max_width(pic, screen::size(), aspect);
    let wanted = args.width.or_else(|| {
        args.height
            .map(|h| ascii::layout(pic.0, pic.1, Fit::Height(h), aspect).0)
    });
    let mut message = notes.into_iter().next();
    let width = match (wanted, fits) {
        (Some(w), Some(most)) if w > most => {
            message = Some(format!("{w} columns don't fit this window; showing {most}"));
            most
        }
        (Some(w), _) => w,
        (None, Some(most)) => most,
        (None, None) => ascii::DEFAULT_COLS,
    };
    let mut tune = Tune::new(
        width.max(LEAST_WIDTH),
        style,
        args.bg,
        colour,
        args.contrast,
        args.brightness,
    );
    tune.message = message;
    tune.colour_allowed = out_shown != Shown::Txt;
    let name = args
        .input
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let (tx, rx) = mpsc::channel::<(Key, Instant)>();
    let mut screen = Screen::enter(false);
    screen.read_keys(move |key, at| tx.send((key, at)).is_ok());

    let mut written: Vec<Outcome> = Vec::new();
    let mut made: Option<(Made, Art)> = None;
    let mut drawn: Option<(u16, u16)> = None;
    let mut dirty = true;
    // When a question reached the screen: only a key read after it answers.
    let mut asked_at: Option<Instant> = None;
    'session: loop {
        let window = screen::size();
        if dirty || drawn != Some(window) {
            redraw(&img, &mut tune, &name, window, aspect, truecolor, &mut made);
            drawn = Some(window);
            dirty = false;
            asked_at = match (&tune.asking, asked_at) {
                (Some(_), None) => Some(Instant::now()),
                (Some(_), shown) => shown,
                (None, _) => None,
            };
        }
        let first = match rx.recv_timeout(RESIZE_CHECK) {
            Ok(key) => key,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Every key already waiting is applied before the next redraw, so
        // a held arrow never leaves the art behind.
        let keys: Vec<(Key, Instant)> = std::iter::once(first).chain(rx.try_iter()).collect();
        for (key, at) in keys {
            // Typed before the question was on screen, so not an answer.
            if tune.asking.is_some() && asked_at.is_none_or(|shown| at < shown) {
                continue;
            }
            let most = max_width(pic, window, aspect).unwrap_or(tune.width);
            let target = match &out {
                Some((path, _)) => path.clone(),
                None => default_target(args, tune.colour)?,
            };
            let was_asking = tune.asking.is_some();
            match tune.press(act(&key), most, &target) {
                Next::Redraw => {}
                Next::Quit => break 'session,
                Next::Copy => tune.message = Some(copy(&img, made.as_ref())),
                Next::Write(path) => {
                    if path.exists() && !args.overwrite && !was_asking {
                        tune.ask_to_replace(path);
                    } else {
                        let shown = shown_for(&path, &out);
                        match write(cli, args, &img, &tune, &path, shown) {
                            Ok(outcome) => {
                                tune.message = Some(format!(
                                    "wrote {} ({})",
                                    file_name(&path),
                                    render::human_size(outcome.bytes)
                                ));
                                written.push(outcome);
                            }
                            Err(e) => tune.message = Some(e.message),
                        }
                    }
                }
            }
            dirty = true;
        }
    }
    drop(screen);

    for outcome in &written {
        if !cli.quiet {
            print!(
                "{}",
                render::conversion_success_human(outcome, render::stdout_styled())
            );
        }
    }
    // The command makes what was written last, or OUT; otherwise it
    // prints, and -o has nothing to do.
    let output = command_output(args.output, written.last().map(|o| o.output.as_path()));
    let shown = match (&out, written.last()) {
        (Some((_, shown)), _) => *shown,
        (None, Some(last)) => shown_for(&last.output, &None),
        (None, None) => Shown::Terminal,
    };
    let how = Invocation {
        input: args.input,
        output: output.as_deref(),
        outdir: args.outdir.filter(|_| output.is_some()),
        default_colour: colour_on(tune.style(), shown, false, false, no_color_env, false).0,
    };
    println!("To make it again: {}", command_line(&tune, &how));
    Ok(())
}

/// Draws the screen, making the art again only when what it is made from
/// changed. A window narrower than the art shrinks it to fit.
fn redraw(
    img: &Image,
    tune: &mut Tune,
    name: &str,
    window: (u16, u16),
    aspect: f32,
    truecolor: bool,
    made: &mut Option<(Made, Art)>,
) {
    let Some(most) = max_width((img.width, img.height), window, aspect) else {
        screen::put(&frame::too_small(window));
        return;
    };
    tune.width = tune.width.min(most);
    let (cols, rows, _) = ascii::layout(img.width, img.height, Fit::Width(tune.width), aspect);
    let want = Made {
        cols,
        rows,
        opts: options(tune, tune.colour, aspect),
    };
    if made.as_ref().is_none_or(|(m, _)| *m != want) {
        let art = ascii::make(img, cols, rows, &want.opts).art;
        *made = Some((want, art));
    }
    let (_, art) = made.as_ref().expect("made just now");
    screen::put(&frame::frame(art, tune, name, window, truecolor));
}

fn options(tune: &Tune, colour: bool, cell_aspect: f32) -> Options {
    Options {
        style: tune.style().clone(),
        bg: tune.bg,
        colour,
        contrast: tune.contrast,
        brightness: tune.brightness,
        cell_aspect,
    }
}

/// Where `w` writes without OUT: NAME-ascii.png for art in colour,
/// NAME-ascii.txt for plain, by the same rules as `conv ascii NAME .png`.
fn default_target(args: &Args, colour: bool) -> Result<PathBuf, ConvError> {
    let ext = if colour { ".png" } else { ".txt" };
    let (path, _) = destination(args.input, Some(Path::new(ext)), args.outdir, false)?;
    Ok(path.expect("a bare extension names a file"))
}

fn shown_for(path: &Path, out: &Option<(PathBuf, Shown)>) -> Shown {
    match out {
        Some((_, shown)) => *shown,
        None if path.extension().is_some_and(|e| e == "png") => Shown::Png,
        None => Shown::Txt,
    }
}

/// Writes the art as it would be written by the command the tuner
/// prints: the same width, laid out for the file's own cell shape.
fn write(
    cli: &Cli,
    args: &Args,
    img: &Image,
    tune: &Tune,
    path: &Path,
    shown: Shown,
) -> Result<Outcome, ConvError> {
    check_drawable(shown, tune.style())?;
    let aspect = cell_aspect(shown);
    let (cols, rows, _) = ascii::layout(img.width, img.height, Fit::Width(tune.width), aspect);
    let opts = options(tune, tune.colour && shown != Shown::Txt, aspect);
    let art = ascii::make(img, cols, rows, &opts).art;
    output::ensure_dir(args.outdir)?;
    let title = args.input.file_name().unwrap_or_default().to_string_lossy();
    // The user has already said yes to replacing it, or it is new.
    write_art(cli, &art, path, shown, tune.bg, &title, true)
}

/// Copies the art shown, as plain text, and says how it went.
fn copy(img: &Image, made: Option<&(Made, Art)>) -> String {
    let Some((m, art)) = made else {
        return "nothing to copy yet".to_string();
    };
    let text = clipboard_art(img, art, &m.opts).text();
    match clipboard::copy(&text) {
        Ok(how) => clipboard::short_note(
            text.chars().count(),
            discord_width(img.width, img.height, 2.0),
            &how,
        ),
        Err(e) => format!("couldn't copy it: {e}"),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
