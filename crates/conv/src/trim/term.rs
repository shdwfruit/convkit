//! The terminal `conv trim` runs in: the way pictures are drawn, chosen
//! from what the terminal says it is, and keys as the session understands
//! them. The session itself is `crate::screen`.

use console::Key;

use super::draw::Graphics;
use super::session::Input;

pub use crate::screen::{put, size};

/// How to draw the picture: `--graphics` when given, otherwise what the
/// environment says the terminal is. Only the environment is read, never
/// the terminal itself: asking it means reading a reply with a timeout,
/// and the reply goes astray under tmux.
pub fn choose(choice: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Graphics {
    let is = |var: &str, value: &str| env(var).is_some_and(|v| v == value);
    let truecolor = crate::ansi::truecolor(env);
    let blocks = Graphics::Blocks { truecolor };
    let sextants = Graphics::Sextants { truecolor };
    match choice {
        Some("blocks") => return blocks,
        Some("sextants") => return sextants,
        Some("kitty") => return Graphics::Kitty,
        Some("iterm") => return Graphics::Iterm,
        _ => {}
    }
    // tmux passes neither protocol through without its own wrapping, but
    // passes characters as they are.
    if env("TMUX").is_some() {
        return sextants;
    }
    if is("TERM", "xterm-kitty") || is("TERM", "xterm-ghostty") || is("TERM_PROGRAM", "ghostty") {
        return Graphics::Kitty;
    }
    if is("TERM_PROGRAM", "iTerm.app") || is("TERM_PROGRAM", "WezTerm") {
        return Graphics::Iterm;
    }
    // Apple's Terminal draws sextants from its fonts, and its own fonts
    // have none; half blocks are in every font.
    if is("TERM_PROGRAM", "Apple_Terminal") {
        return blocks;
    }
    sextants
}

/// A key as the session understands it.
pub fn input_for(key: &Key) -> Input {
    match key {
        Key::ArrowLeft => Input::Left,
        Key::ArrowRight => Input::Right,
        Key::ArrowUp => Input::Up,
        Key::ArrowDown => Input::Down,
        Key::PageUp => Input::PageUp,
        Key::PageDown => Input::PageDown,
        Key::Home => Input::Home,
        Key::End => Input::End,
        Key::Enter => Input::Enter,
        Key::Escape => Input::Escape,
        Key::CtrlC | Key::Char('\u{3}') => Input::Quit,
        Key::Char(c) => match c.to_ascii_lowercase() {
            'c' => Input::Cut,
            'u' => Input::Undo,
            'w' => Input::Write,
            'q' => Input::Quit,
            'y' => Input::Yes,
            '+' | '=' => Input::ZoomIn,
            '-' | '_' => Input::ZoomOut,
            ',' | '<' => Input::FrameBack,
            '.' | '>' => Input::FrameForward,
            _ => Input::Other,
        },
        _ => Input::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_terminal_is_told_from_its_environment() {
        let cases: &[(&[(&str, &str)], Graphics)] = &[
            (&[("TERM", "xterm-kitty")], Graphics::Kitty),
            (&[("TERM", "xterm-ghostty")], Graphics::Kitty),
            (&[("TERM_PROGRAM", "ghostty")], Graphics::Kitty),
            (&[("TERM_PROGRAM", "iTerm.app")], Graphics::Iterm),
            (&[("TERM_PROGRAM", "WezTerm")], Graphics::Iterm),
            (
                &[("COLORTERM", "truecolor")],
                Graphics::Sextants { truecolor: true },
            ),
            (
                &[("COLORTERM", "24bit")],
                Graphics::Sextants { truecolor: true },
            ),
            (
                &[("WT_SESSION", "x")],
                Graphics::Sextants { truecolor: true },
            ),
            (&[], Graphics::Sextants { truecolor: false }),
            // Apple's Terminal draws sextants from its fonts, which lack them.
            (
                &[("TERM_PROGRAM", "Apple_Terminal")],
                Graphics::Blocks { truecolor: false },
            ),
            // tmux would need its own escapes passed through, but passes
            // characters as they are.
            (
                &[
                    ("TMUX", "/tmp/s"),
                    ("TERM", "xterm-kitty"),
                    ("COLORTERM", "truecolor"),
                ],
                Graphics::Sextants { truecolor: true },
            ),
        ];
        for (pairs, want) in cases {
            assert_eq!(choose(None, &env(pairs)), *want, "{pairs:?}");
        }
    }

    #[test]
    fn graphics_overrides_what_the_environment_says() {
        let kitty = env(&[("TERM", "xterm-kitty"), ("COLORTERM", "truecolor")]);
        assert_eq!(
            choose(Some("blocks"), &kitty),
            Graphics::Blocks { truecolor: true }
        );
        assert_eq!(
            choose(Some("sextants"), &kitty),
            Graphics::Sextants { truecolor: true }
        );
        assert_eq!(choose(Some("iterm"), &kitty), Graphics::Iterm);
        assert_eq!(choose(Some("kitty"), &env(&[])), Graphics::Kitty);
    }

    #[test]
    fn keys_map_to_what_they_do() {
        let cases = [
            (Key::ArrowLeft, Input::Left),
            (Key::ArrowRight, Input::Right),
            (Key::ArrowUp, Input::Up),
            (Key::ArrowDown, Input::Down),
            (Key::PageUp, Input::PageUp),
            (Key::PageDown, Input::PageDown),
            (Key::Home, Input::Home),
            (Key::End, Input::End),
            (Key::Enter, Input::Enter),
            (Key::Escape, Input::Escape),
            (Key::CtrlC, Input::Quit),
            (Key::Char('\u{3}'), Input::Quit),
            (Key::Char('c'), Input::Cut),
            (Key::Char('u'), Input::Undo),
            (Key::Char('w'), Input::Write),
            (Key::Char('q'), Input::Quit),
            (Key::Char('y'), Input::Yes),
            (Key::Char('Y'), Input::Yes),
            (Key::Char('+'), Input::ZoomIn),
            (Key::Char('='), Input::ZoomIn),
            (Key::Char('-'), Input::ZoomOut),
            (Key::Char(','), Input::FrameBack),
            (Key::Char('.'), Input::FrameForward),
            (Key::Char('x'), Input::Other),
            (Key::Tab, Input::Other),
        ];
        for (key, want) in cases {
            assert_eq!(input_for(&key), want, "{key:?}");
        }
    }
}
