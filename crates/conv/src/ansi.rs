//! Colour escape codes for the terminal, shared by `conv trim` (its
//! picture and bars) and `conv ascii` (coloured art).

use convkit_core::ascii::{Art, Cell};

/// The SGR parameters for text in colour `c`: 24-bit where the terminal
/// has it, the nearest of the 256-colour palette otherwise.
pub fn fg_params(c: [u8; 3], truecolor: bool) -> String {
    if truecolor {
        format!("38;2;{};{};{}", c[0], c[1], c[2])
    } else {
        format!("38;5;{}", ansi256(c[0], c[1], c[2]))
    }
}

/// Text in colour `c`.
pub fn fg(c: [u8; 3], truecolor: bool) -> String {
    format!("\x1b[{}m", fg_params(c, truecolor))
}

/// The background in colour `c`, or the terminal's own for `None`.
pub fn bg(c: Option<[u8; 3]>, truecolor: bool) -> String {
    match c {
        // The default background, for the half of a cell with no pixel.
        None => "\x1b[49m".to_string(),
        Some(c) if truecolor => format!("\x1b[48;2;{};{};{}m", c[0], c[1], c[2]),
        Some(c) => format!("\x1b[48;5;{}m", ansi256(c[0], c[1], c[2])),
    }
}

/// The xterm 256-colour index nearest an RGB colour: the 6x6x6 cube or the
/// grey ramp, whichever is closer.
pub fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        (0..6)
            .min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(v)).abs())
            .expect("six levels")
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let dist = |c: [u8; 3]| {
        [r, g, b]
            .iter()
            .zip(c)
            .map(|(&a, b)| (i32::from(a) - i32::from(b)).pow(2))
            .sum::<i32>()
    };
    let cube = dist([LEVELS[ri], LEVELS[gi], LEVELS[bi]]);
    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let step = (mean.saturating_sub(8) / 10).min(23) as u8;
    let grey_value = 8 + 10 * step;
    if dist([grey_value; 3]) < cube {
        232 + step
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

/// Whether the terminal draws 24-bit colour, from its environment:
/// `COLORTERM` says so, or it is Windows Terminal (`WT_SESSION`), which
/// always does but doesn't set `COLORTERM`.
pub fn truecolor(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let is = |var: &str, value: &str| env(var).is_some_and(|v| v == value);
    is("COLORTERM", "truecolor") || is("COLORTERM", "24bit") || env("WT_SESSION").is_some()
}

/// The art in the terminal's colours: each character with its own colour
/// and its cell's background, an escape only where those change, and a
/// reset at the end of every line that used one. A missing colour is the
/// terminal's own. Plain cells at the end of a line are left off.
pub fn paint(art: &Art, truecolor: bool) -> String {
    let mut s = String::new();
    for row in art.cells.chunks(art.cols.max(1) as usize) {
        let plain = |c: &Cell| c.ch == ' ' && c.fg.is_none() && c.bg.is_none();
        let end = row.iter().rposition(|c| !plain(c)).map_or(0, |i| i + 1);
        let mut last = None;
        for cell in &row[..end] {
            let pair = (cell.fg, cell.bg);
            if (last.is_some() || pair != (None, None)) && last != Some(pair) {
                s.push_str(&match cell.fg {
                    Some(c) => fg(c, truecolor),
                    None => "\x1b[39m".to_string(),
                });
                s.push_str(&bg(cell.bg, truecolor));
                last = Some(pair);
            }
            s.push(cell.ch);
        }
        if last.is_some() {
            s.push_str("\x1b[0m");
        }
        s.push('\n');
    }
    s
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
    fn ansi256_picks_the_nearest_cube_or_grey() {
        assert_eq!(ansi256(0, 0, 0), 16);
        assert_eq!(ansi256(255, 255, 255), 231);
        assert_eq!(ansi256(255, 0, 0), 196);
        assert_eq!(ansi256(128, 128, 128), 244);
        assert_eq!(ansi256(0, 135, 255), 33);
    }

    #[test]
    fn truecolor_is_told_from_colorterm_or_windows_terminal() {
        assert!(truecolor(&env(&[("COLORTERM", "truecolor")])));
        assert!(truecolor(&env(&[("WT_SESSION", "1")])));
        assert!(!truecolor(&env(&[])));
    }
    #[test]
    fn paint_uses_lower_half_and_resets() {
        use convkit_core::ascii::{Art, Cell};
        let art = Art {
            cols: 2,
            rows: 1,
            cells: vec![
                Cell {
                    ch: '▀',
                    fg: Some([255, 0, 0]),
                    bg: Some([0, 0, 255]),
                },
                Cell {
                    ch: '▄',
                    fg: Some([0, 0, 255]),
                    bg: None,
                },
            ],
        };
        assert_eq!(
            paint(&art, true),
            "\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m▀\x1b[38;2;0;0;255m\x1b[49m▄\x1b[0m\n"
        );
        let plain = Art {
            cols: 3,
            rows: 1,
            cells: vec![
                Cell {
                    ch: '@',
                    fg: None,
                    bg: None
                };
                3
            ],
        };
        assert_eq!(paint(&plain, true), "@@@\n");
    }
}
