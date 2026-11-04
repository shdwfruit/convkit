//! `conv colors PICTURE`: the main colours of a picture, or the colour of
//! one pixel, as HEX, RGB and HSL, or as code.
//!
//! Input works as in the text tools: a file, `-` or a pipe for a picture
//! piped in, or `--paste`. The result goes to stdout, with swatches only on
//! a terminal that shows colour, `--copy` also puts it on the clipboard,
//! and `--json` prints one JSON document.

use std::io::IsTerminal;

use convkit_core::palette::{self, Code, Colour};
use convkit_core::{ConvError, ErrorCode};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::{ansi, clipboard, paste, picture, render};

/// Columns the bar of shares takes above a palette.
const BAR: usize = 48;

pub fn run(cli: &Cli) -> i32 {
    match colors(cli) {
        Ok(()) => 0,
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

fn colors(cli: &Cli) -> Result<(), ConvError> {
    let Some(Command::Colors {
        input,
        count,
        at,
        code,
        copy,
        paste,
    }) = &cli.command
    else {
        unreachable!("main dispatches only Command::Colors here");
    };
    // The folder a pasted picture is in lives until it is read.
    let pasted = paste.then(|| paste::one("conv colors")).transpose()?;
    let (read, shown_as) =
        match (&pasted, input) {
            (Some(p), _) => {
                let name = if p.homeless() {
                    paste::PASTED.to_string()
                } else {
                    p.path.display().to_string()
                };
                (picture::read(cli, &p.path, "conv colors", 1), name)
            }
            (None, Some(p)) if p.as_os_str() == "-" => {
                (picture::read_stdin(cli, "conv colors", 1), "-".to_string())
            }
            (None, Some(p)) => (
                picture::read(cli, p, "conv colors", 1),
                p.display().to_string(),
            ),
            (None, None) if !std::io::stdin().is_terminal() => {
                (picture::read_stdin(cli, "conv colors", 1), "-".to_string())
            }
            (None, None) => return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "give conv colors a picture: conv colors photo.jpg, - for one piped in, or --paste",
            )),
        };
    let read = read?;
    let img = &read.picture.image;
    let entries: Vec<(Colour, Option<f32>)> = match at {
        Some((x, y)) => vec![(palette::at(img, *x, *y)?, None)],
        None => palette::palette(img, usize::from(*count))?
            .into_iter()
            .map(|s| (s.colour, Some(s.share)))
            .collect(),
    };
    let code_text = code.map(|c| palette::code(&entries, c));
    let copied = copy.then(|| {
        let text = match &code_text {
            Some(t) => t.clone(),
            None => entries
                .iter()
                .map(|(c, _)| c.hex())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        copy_text(&text, &entries, *code)
    });

    if cli.json {
        let mut v = json!({
            "ok": true,
            "input": shown_as,
            "width": img.width,
            "height": img.height,
            "colors": entries.iter().map(|(c, share)| colour_json(c, *share)).collect::<Vec<_>>(),
            "notes": read.notes,
        });
        if let Some((x, y)) = at {
            v["at"] = json!({ "x": x, "y": y });
        }
        if let (Some(kind), Some(text)) = (code, &code_text) {
            v["code"] = json!({ "language": kind.name(), "text": text });
        }
        if let Some((ok, message)) = &copied {
            v["clipboard"] = json!({ "ok": ok, "message": message });
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return Ok(());
    }
    match &code_text {
        Some(t) => print!("{t}"),
        None => print!("{}", human(&entries, at.is_none(), swatches())),
    }
    eprint!(
        "{}",
        render::notes_human(&read.notes, render::stderr_styled())
    );
    if let Some((ok, message)) = copied {
        if !ok {
            let line = format!("warning  {message}");
            eprintln!("{}", render::paint_warning(&line, render::stderr_styled()));
        } else if !cli.quiet {
            eprintln!("{message}");
        }
    }
    Ok(())
}

/// How swatches can be drawn: not at all off a terminal or under
/// `NO_COLOR`, and in 24-bit colour where the terminal has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swatches {
    Off,
    On { truecolor: bool },
}

fn swatches() -> Swatches {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    if no_color || !render::stdout_styled() {
        return Swatches::Off;
    }
    Swatches::On {
        truecolor: ansi::truecolor(&|k| std::env::var(k).ok()),
    }
}

fn colour_json(c: &Colour, share: Option<f32>) -> serde_json::Value {
    let mut v = json!({
        "hex": c.hex(),
        "rgb": [c.r, c.g, c.b],
        "hsl": c.hsl(),
    });
    if c.a < 255 {
        v["alpha"] = json!(c.a);
    }
    if let Some(s) = share {
        v["share"] = json!((f64::from(s) * 10_000.0).round() / 10_000.0);
    }
    v
}

/// One line a colour, its columns lined up, after a bar of the shares for
/// a palette when swatches can be drawn.
fn human(entries: &[(Colour, Option<f32>)], bar: bool, swatches: Swatches) -> String {
    let cols: Vec<[String; 3]> = entries
        .iter()
        .map(|(c, _)| [c.hex(), c.rgb_css(), c.hsl_css()])
        .collect();
    let width = |i: usize| cols.iter().map(|c| c[i].len()).max().unwrap_or(0);
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let mut s = String::new();
    if let (true, Swatches::On { truecolor }) = (bar && entries.len() > 1, swatches) {
        s.push_str(&share_bar(entries, truecolor));
        s.push('\n');
    }
    for ((c, share), [hex, rgb, hsl]) in entries.iter().zip(&cols) {
        let mut line = String::new();
        if let Swatches::On { truecolor } = swatches {
            line.push_str(&ansi::bg(Some([c.r, c.g, c.b]), truecolor));
            line.push_str("      \x1b[0m  ");
        }
        line.push_str(&format!("{hex:<w0$}  {rgb:<w1$}  {hsl:<w2$}"));
        if let Some(share) = share {
            line.push_str(&format!("  {:>4}", palette::percent(*share)));
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

/// The shares side by side, each as wide as its part of the picture.
fn share_bar(entries: &[(Colour, Option<f32>)], truecolor: bool) -> String {
    let mut s = String::new();
    let mut used = 0usize;
    let mut cumulative = 0f32;
    for (c, share) in entries {
        cumulative += share.unwrap_or(0.0);
        let upto = ((cumulative * BAR as f32).round() as usize).min(BAR);
        let n = upto.saturating_sub(used);
        used += n;
        if n > 0 {
            s.push_str(&ansi::bg(Some([c.r, c.g, c.b]), truecolor));
            s.push_str(&" ".repeat(n));
        }
    }
    s.push_str("\x1b[0m");
    s
}

/// Copies `text` and says what was copied.
fn copy_text(text: &str, entries: &[(Colour, Option<f32>)], code: Option<Code>) -> (bool, String) {
    let what = match (code, entries) {
        (Some(c), _) => format!("the {} code", code_name(c)),
        (None, [(c, _)]) => c.hex(),
        (None, many) => format!("{} colours", many.len()),
    };
    match clipboard::copy(text) {
        Ok(clipboard::Copied::Osc52) => (
            true,
            format!("asked the terminal to copy {what}, which some terminals don't allow"),
        ),
        Ok(_) => (true, format!("copied {what} to the clipboard")),
        Err(e) => (false, format!("couldn't copy to the clipboard: {e}")),
    }
}

fn code_name(c: Code) -> &'static str {
    match c {
        Code::Css => "CSS",
        Code::Swiftui => "SwiftUI",
        Code::Tailwind => "Tailwind",
        Code::Tailwind3 => "Tailwind 3",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<(Colour, Option<f32>)> {
        vec![
            (Colour::rgb(43, 74, 111), Some(0.62)),
            (Colour::rgb(255, 255, 255), Some(0.38)),
        ]
    }

    #[test]
    fn plain_lines_line_up_without_escape_codes() {
        let text = human(&entries(), true, Swatches::Off);
        assert_eq!(
            text,
            "#2b4a6f  rgb(43, 74, 111)    hsl(213, 44%, 30%)   62%\n\
             #ffffff  rgb(255, 255, 255)  hsl(0, 0%, 100%)     38%\n"
        );
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn swatches_and_a_bar_on_a_terminal() {
        let text = human(&entries(), true, Swatches::On { truecolor: true });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("\x1b[48;2;43;74;111m"), "{text}");
        // 62% of the bar, then the rest.
        assert!(
            lines[0].contains(&format!(
                "\x1b[48;2;43;74;111m{}\x1b[48;2;255;255;255m",
                " ".repeat(30)
            )),
            "{text}"
        );
        assert!(
            lines[1].starts_with("\x1b[48;2;43;74;111m      \x1b[0m  #2b4a6f"),
            "{text}"
        );
    }

    #[test]
    fn one_pixel_has_no_share_and_no_bar() {
        let one = [(Colour::rgb(1, 2, 3), None)];
        let text = human(&one, false, Swatches::On { truecolor: false });
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.ends_with("#010203  rgb(1, 2, 3)  hsl(210, 50%, 1%)\n"),
            "{text}"
        );
    }

    #[test]
    fn json_colours_carry_alpha_only_when_it_matters() {
        let v = colour_json(&Colour::rgb(43, 74, 111), Some(0.623_456));
        assert_eq!(v["hex"], "#2b4a6f");
        assert_eq!(v["share"], 0.6235);
        assert!(v.get("alpha").is_none());
        let v = colour_json(
            &Colour {
                a: 10,
                ..Colour::rgb(1, 2, 3)
            },
            None,
        );
        assert_eq!(v["alpha"], 10);
        assert!(v.get("share").is_none());
    }
}
