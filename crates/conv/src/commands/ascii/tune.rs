//! The tuner's state (`conv ascii -i`): what each key changes, and the
//! `conv ascii` command for what it shows. Nothing here touches the
//! terminal; `frame` draws it and `ascii.rs` runs the session.

use std::path::{Path, PathBuf};

use console::Key;
use convkit_core::ascii::{Bg, Style};

use crate::render::shell_quote;

/// What a key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Narrower,
    Wider,
    MoreContrast,
    LessContrast,
    Brighter,
    Darker,
    NextStyle,
    FlipBg,
    ToggleColour,
    Write,
    /// `y`: yes to a question, otherwise copy.
    Yes,
    Quit,
    Other,
}

pub fn act(key: &Key) -> Act {
    match key {
        Key::ArrowLeft => Act::Narrower,
        Key::ArrowRight => Act::Wider,
        Key::ArrowUp => Act::MoreContrast,
        Key::ArrowDown => Act::LessContrast,
        Key::Escape | Key::CtrlC | Key::Char('\u{3}') => Act::Quit,
        Key::Char(c) => match c.to_ascii_lowercase() {
            ']' => Act::Brighter,
            '[' => Act::Darker,
            'c' => Act::NextStyle,
            'b' => Act::FlipBg,
            'o' => Act::ToggleColour,
            'w' => Act::Write,
            'y' => Act::Yes,
            'q' => Act::Quit,
            _ => Act::Other,
        },
        _ => Act::Other,
    }
}

/// What the session does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Redraw,
    Write(PathBuf),
    Copy,
    Quit,
}

/// The narrowest the art gets.
pub const LEAST_WIDTH: u32 = 8;
/// How far one press moves contrast or brightness.
const STEP: i32 = 10;

#[derive(Debug, Clone)]
pub struct Tune {
    pub width: u32,
    pub contrast: i32,
    pub brightness: i32,
    /// ascii, blocks, braille, then the user's own ramp if they gave one.
    pub styles: Vec<Style>,
    pub style: usize,
    pub bg: Bg,
    pub colour: bool,
    /// Shown in place of the keys until the next key.
    pub message: Option<String>,
    /// The file a `y` would replace.
    pub asking: Option<PathBuf>,
    /// Whether colour was set by hand with `o`, so a new style keeps it
    /// instead of taking its own default.
    colour_set: bool,
}

impl Tune {
    pub fn new(
        width: u32,
        style: Style,
        bg: Bg,
        colour: bool,
        contrast: i32,
        brightness: i32,
    ) -> Tune {
        let mut styles = vec![Style::Ascii, Style::Blocks, Style::Braille];
        if let Style::Ramp(_) = style {
            styles.push(style.clone());
        }
        let style = styles.iter().position(|s| *s == style).unwrap_or(0);
        Tune {
            width,
            contrast,
            brightness,
            styles,
            style,
            bg,
            colour,
            message: None,
            asking: None,
            colour_set: false,
        }
    }

    pub fn style(&self) -> &Style {
        &self.styles[self.style]
    }

    /// Applies `act`. `max_width` is the widest art the window shows, and
    /// `target` the file `w` writes.
    pub fn press(&mut self, act: Act, max_width: u32, target: &Path) -> Next {
        self.message = None;
        if let Some(path) = self.asking.take() {
            if act == Act::Yes {
                return Next::Write(path);
            }
            self.message = Some(format!("kept {}", name(&path)));
            return Next::Redraw;
        }
        let max_width = max_width.max(LEAST_WIDTH);
        match act {
            Act::Narrower => {
                self.width = self.width.saturating_sub(1).clamp(LEAST_WIDTH, max_width)
            }
            Act::Wider => self.width = (self.width + 1).clamp(LEAST_WIDTH, max_width),
            Act::MoreContrast => self.contrast = (self.contrast + STEP).min(100),
            Act::LessContrast => self.contrast = (self.contrast - STEP).max(-100),
            Act::Brighter => self.brightness = (self.brightness + STEP).min(100),
            Act::Darker => self.brightness = (self.brightness - STEP).max(-100),
            Act::NextStyle => {
                self.style = (self.style + 1) % self.styles.len();
                if !self.colour_set {
                    self.colour = *self.style() == Style::Blocks;
                }
            }
            Act::FlipBg => {
                self.bg = match self.bg {
                    Bg::Dark => Bg::Light,
                    Bg::Light => Bg::Dark,
                }
            }
            Act::ToggleColour => {
                self.colour = !self.colour;
                self.colour_set = true;
            }
            Act::Write => return Next::Write(target.to_path_buf()),
            Act::Yes => return Next::Copy,
            Act::Quit => return Next::Quit,
            Act::Other => {}
        }
        Next::Redraw
    }

    /// Asks before `w` replaces `path`.
    pub fn ask_to_replace(&mut self, path: PathBuf) {
        self.message = Some(format!("{} exists; replace it? y/n", name(&path)));
        self.asking = Some(path);
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// How conv was run, for the command the tuner prints.
#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    pub input: &'a Path,
    pub output: Option<&'a Path>,
    pub outdir: Option<&'a Path>,
    /// Whether the style the tuner ends on is in colour without `--color`
    /// where this command's art goes.
    pub default_colour: bool,
}

/// The `conv ascii` command that makes what the tuner shows: the input,
/// OUT and `-o` as given, the width, then only what differs from the
/// defaults.
pub fn command_line(tune: &Tune, how: &Invocation) -> String {
    let mut words = vec!["conv".to_string(), "ascii".to_string()];
    words.push(shell_quote(&how.input.to_string_lossy()));
    if let Some(out) = how.output {
        words.push(shell_quote(&out.to_string_lossy()));
    }
    if let Some(dir) = how.outdir {
        words.push("-o".into());
        words.push(shell_quote(&dir.to_string_lossy()));
    }
    words.push(format!("--width {}", tune.width));
    match tune.style() {
        Style::Ascii => {}
        Style::Ramp(chars) => {
            let ramp: String = chars.iter().collect();
            words.push(format!("--chars {}", shell_quote(&ramp)));
        }
        style => words.push(format!("--chars {}", style.name())),
    }
    if tune.bg == Bg::Light {
        words.push("--bg light".into());
    }
    match (tune.colour, how.default_colour) {
        (true, false) => words.push("--color".into()),
        (false, true) => words.push("--no-color".into()),
        _ => {}
    }
    if tune.contrast != 0 {
        words.push(format!("--contrast {}", tune.contrast));
    }
    if tune.brightness != 0 {
        words.push(format!("--brightness {}", tune.brightness));
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::Key;

    fn tune() -> Tune {
        Tune::new(10, Style::Ascii, Bg::Dark, false, 0, 0)
    }

    fn press(t: &mut Tune, act: Act) -> Next {
        t.press(act, 400, Path::new("cat-ascii.png"))
    }

    #[test]
    fn keys_map_to_acts() {
        let cases = [
            (Key::ArrowLeft, Act::Narrower),
            (Key::ArrowRight, Act::Wider),
            (Key::ArrowUp, Act::MoreContrast),
            (Key::ArrowDown, Act::LessContrast),
            (Key::Char(']'), Act::Brighter),
            (Key::Char('['), Act::Darker),
            (Key::Char('c'), Act::NextStyle),
            (Key::Char('b'), Act::FlipBg),
            (Key::Char('o'), Act::ToggleColour),
            (Key::Char('w'), Act::Write),
            (Key::Char('y'), Act::Yes),
            (Key::Char('q'), Act::Quit),
            (Key::Char('Q'), Act::Quit),
            (Key::Escape, Act::Quit),
            (Key::CtrlC, Act::Quit),
            (Key::Char('\u{3}'), Act::Quit),
            (Key::Char('x'), Act::Other),
            (Key::Tab, Act::Other),
        ];
        for (key, want) in cases {
            assert_eq!(act(&key), want, "{key:?}");
        }
    }

    #[test]
    fn width_steps_and_stays_in_range() {
        let mut t = tune();
        for _ in 0..5 {
            press(&mut t, Act::Narrower);
        }
        assert_eq!(t.width, 8);
        for _ in 0..10 {
            t.press(Act::Wider, 12, Path::new("a.png"));
        }
        assert_eq!(t.width, 12);
    }

    #[test]
    fn contrast_and_brightness_step_by_ten_and_clamp() {
        let mut t = Tune::new(10, Style::Ascii, Bg::Dark, false, 95, -95);
        press(&mut t, Act::MoreContrast);
        press(&mut t, Act::Darker);
        assert_eq!((t.contrast, t.brightness), (100, -100));
        press(&mut t, Act::LessContrast);
        press(&mut t, Act::Brighter);
        assert_eq!((t.contrast, t.brightness), (90, -90));
    }

    #[test]
    fn styles_cycle_and_bring_their_colour() {
        let mut t = tune();
        press(&mut t, Act::NextStyle);
        assert_eq!((t.style(), t.colour), (&Style::Blocks, true));
        press(&mut t, Act::NextStyle);
        assert_eq!((t.style(), t.colour), (&Style::Braille, false));
        press(&mut t, Act::NextStyle);
        assert_eq!(t.style(), &Style::Ascii);

        let ramp = Style::parse(" .oO@").unwrap();
        let mut t = Tune::new(10, ramp.clone(), Bg::Dark, false, 0, 0);
        assert_eq!(t.styles.len(), 4);
        assert_eq!(t.style(), &ramp);
        press(&mut t, Act::NextStyle);
        assert_eq!(t.style(), &Style::Ascii);

        // Colour set by hand stays as set.
        let mut t = tune();
        press(&mut t, Act::ToggleColour);
        press(&mut t, Act::NextStyle);
        press(&mut t, Act::NextStyle);
        assert_eq!((t.style(), t.colour), (&Style::Braille, true));
    }

    #[test]
    fn b_flips_the_background() {
        let mut t = tune();
        press(&mut t, Act::FlipBg);
        assert_eq!(t.bg, Bg::Light);
        press(&mut t, Act::FlipBg);
        assert_eq!(t.bg, Bg::Dark);
    }

    #[test]
    fn w_writes_to_the_target() {
        let mut t = tune();
        assert_eq!(
            press(&mut t, Act::Write),
            Next::Write(PathBuf::from("cat-ascii.png"))
        );
    }

    #[test]
    fn asking_takes_y_as_yes_and_anything_else_as_no() {
        let mut t = tune();
        t.ask_to_replace(PathBuf::from("a.png"));
        assert_eq!(t.message.as_deref(), Some("a.png exists; replace it? y/n"));
        assert_eq!(press(&mut t, Act::Yes), Next::Write(PathBuf::from("a.png")));
        assert_eq!(t.asking, None);

        t.ask_to_replace(PathBuf::from("a.png"));
        assert_eq!(press(&mut t, Act::Other), Next::Redraw);
        assert_eq!(t.asking, None);
        assert_eq!(t.message.as_deref(), Some("kept a.png"));
        // No other key acts while asking: q only answers no.
        t.ask_to_replace(PathBuf::from("a.png"));
        assert_eq!(press(&mut t, Act::Quit), Next::Redraw);
    }

    #[test]
    fn y_copies_when_not_asking() {
        assert_eq!(press(&mut tune(), Act::Yes), Next::Copy);
    }

    #[test]
    fn any_key_clears_the_message() {
        let mut t = tune();
        t.message = Some("wrote a.png".into());
        press(&mut t, Act::Wider);
        assert_eq!(t.message, None);
    }

    #[test]
    fn command_line_names_only_what_changed() {
        let how = Invocation {
            input: Path::new("cat.jpg"),
            output: None,
            outdir: None,
            default_colour: false,
        };
        let mut t = Tune::new(60, Style::Ascii, Bg::Dark, false, 0, 0);
        assert_eq!(command_line(&t, &how), "conv ascii cat.jpg --width 60");

        t = Tune::new(60, Style::Blocks, Bg::Light, false, 20, 0);
        let blocks = Invocation {
            default_colour: true,
            ..how
        };
        assert_eq!(
            command_line(&t, &blocks),
            "conv ascii cat.jpg --width 60 --chars blocks --bg light --no-color --contrast 20"
        );

        t = Tune::new(60, Style::Braille, Bg::Dark, true, 0, -30);
        assert_eq!(
            command_line(&t, &how),
            "conv ascii cat.jpg --width 60 --chars braille --color --brightness -30"
        );

        t = Tune::new(60, Style::parse(" .oO@").unwrap(), Bg::Dark, false, 0, 0);
        let spaced = Invocation {
            input: Path::new("my cat.jpg"),
            ..how
        };
        let line = command_line(&t, &spaced);
        // Quoted the way this platform's shells take it.
        assert!(line.contains(&shell_quote("my cat.jpg")), "{line}");
        assert!(
            line.contains(&format!("--chars {}", shell_quote(" .oO@"))),
            "{line}"
        );
        assert!(
            line.contains(' ') && line.matches(".oO@").count() == 1,
            "{line}"
        );

        t = Tune::new(60, Style::Ascii, Bg::Dark, false, 0, 0);
        let out = Invocation {
            output: Some(Path::new("art.png")),
            outdir: Some(Path::new("out")),
            ..how
        };
        assert_eq!(
            command_line(&t, &out),
            "conv ascii cat.jpg art.png -o out --width 60"
        );
    }
}
