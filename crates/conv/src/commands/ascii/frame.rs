//! The tuner's screen: the art on the page it is made for, and two
//! status rows at the bottom. Built as one string, written in one go.

use convkit_core::ascii::{self, Art, Bg, Fit};

use super::tune::Tune;
use crate::ansi;

/// Rows under the art: the settings, then the keys or a message.
pub const STATUS_ROWS: u16 = 2;
/// The smallest window the tuner draws in, columns then rows.
pub const LEAST: (u16, u16) = (24, 8);

/// The keys, longest first: the first that fits the window is shown, so
/// `q quit` is always there.
const KEYS: [&str; 3] = [
    "←→ width  ↑↓ contrast  [ ] brightness  c style  b background  o colour  w write  y copy  q quit",
    "←→ width ↑↓ contrast [] brightness c style b page o colour w write y copy q quit",
    "q quit  w write  c style",
];

/// The widest art a `pic` (width, height) shows whole in a `window`
/// (columns, rows) above the status rows, or `None` when the window is
/// smaller than `LEAST`.
pub fn max_width(pic: (u32, u32), window: (u16, u16), cell_aspect: f32) -> Option<u32> {
    let (cols, rows) = window;
    if cols < LEAST.0 || rows < LEAST.1 {
        return None;
    }
    let fit = Fit::Box(u32::from(cols), u32::from(rows - STATUS_ROWS));
    let (width, _, _) = ascii::layout(pic.0, pic.1, fit, cell_aspect);
    Some(width.max(super::tune::LEAST_WIDTH))
}

/// The art on its page: every missing colour filled with the page's text
/// and background colours, so the preview looks as it will where it is
/// shown, a light page on a dark terminal too.
pub fn on_page(art: &Art, bg: Bg) -> Art {
    let mut art = art.clone();
    for cell in &mut art.cells {
        cell.fg = cell.fg.or(Some(bg.text()));
        cell.bg = cell.bg.or(Some(bg.page()));
    }
    art
}

/// The whole screen for `window` (columns, rows).
pub fn frame(art: &Art, tune: &Tune, name: &str, window: (u16, u16), truecolor: bool) -> String {
    let (cols, rows) = window;
    let room = usize::from(rows.saturating_sub(STATUS_ROWS));
    let mut out = String::new();
    let painted = ansi::paint(&on_page(art, tune.bg), truecolor);
    for (r, line) in painted.lines().take(room).enumerate() {
        out.push_str(&format!("\x1b[{};1H{line}\x1b[0m\x1b[K", r + 1));
    }
    out.push_str("\x1b[J");
    let settings = format!(
        "{name}  {}x{}  {}  {}  colour {}  contrast {}  brightness {}",
        art.cols,
        art.rows,
        tune.style().name(),
        match tune.bg {
            Bg::Dark => "dark",
            Bg::Light => "light",
        },
        if tune.colour { "on" } else { "off" },
        tune.contrast,
        tune.brightness,
    );
    let keys = KEYS
        .iter()
        .find(|k| k.chars().count() <= usize::from(cols))
        .unwrap_or(&KEYS[2]);
    let below = tune.message.as_deref().unwrap_or(keys);
    for (k, text) in [settings.as_str(), below].iter().enumerate() {
        let row = rows - STATUS_ROWS + 1 + k as u16;
        let cut: String = text.chars().take(usize::from(cols)).collect();
        out.push_str(&format!("\x1b[{row};1H\x1b[2K{cut}"));
    }
    out
}

/// What shows in a window smaller than `LEAST`.
pub fn too_small(window: (u16, u16)) -> String {
    let text = format!(
        "conv ascii -i needs a window at least {} columns wide and {} rows high; make it bigger, or press q",
        LEAST.0, LEAST.1
    );
    let cut: String = text
        .chars()
        .take(usize::from(window.0) * usize::from(window.1))
        .collect();
    format!("\x1b[2J\x1b[1;1H{cut}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use convkit_core::ascii::{Cell, Style};

    fn art(cols: u32, rows: u32) -> Art {
        Art {
            cols,
            rows,
            cells: vec![
                Cell {
                    ch: '@',
                    fg: None,
                    bg: None,
                };
                (cols * rows) as usize
            ],
        }
    }

    fn tune() -> Tune {
        Tune::new(3, Style::Ascii, Bg::Dark, false, 0, 0)
    }

    /// The text of each status row: what follows each `\x1b[2K`.
    fn status_rows(frame: &str) -> Vec<String> {
        frame
            .split("\x1b[2K")
            .skip(1)
            .map(|s| s.split('\x1b').next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn max_width_fits_the_window() {
        assert_eq!(max_width((100, 100), (80, 24), 2.0), Some(44));
        assert_eq!(max_width((100, 100), (23, 24), 2.0), None);
        assert_eq!(max_width((100, 100), (80, 7), 2.0), None);
        assert_eq!(max_width((400, 100), (80, 24), 2.0), Some(80));
    }

    #[test]
    fn on_page_fills_missing_colours() {
        let mut a = art(2, 1);
        a.cells[1].fg = Some([1, 2, 3]);
        a.cells[1].bg = Some([4, 5, 6]);
        let painted = on_page(&a, Bg::Light);
        assert_eq!(painted.cells[0].fg, Some([0x18, 0x18, 0x1b]));
        assert_eq!(painted.cells[0].bg, Some([0xff, 0xff, 0xff]));
        assert_eq!(painted.cells[1].fg, Some([1, 2, 3]));
        assert_eq!(painted.cells[1].bg, Some([4, 5, 6]));
    }

    #[test]
    fn frame_puts_the_art_on_top_and_status_at_the_bottom() {
        let mut t = tune();
        let f = frame(&art(3, 2), &t, "cat.jpg", (100, 10), true);
        for at in ["\x1b[1;1H", "\x1b[2;1H", "\x1b[9;1H", "\x1b[10;1H"] {
            assert!(f.contains(at), "{at:?} in {f:?}");
        }
        let rows = status_rows(&f);
        assert!(
            rows[0].contains("cat.jpg") && rows[0].contains("3x2"),
            "{rows:?}"
        );
        assert!(rows[1].contains("q quit"), "{rows:?}");

        t.message = Some("wrote a.png".into());
        let rows = status_rows(&frame(&art(3, 2), &t, "cat.jpg", (100, 10), true));
        assert_eq!(rows[1], "wrote a.png");
    }

    #[test]
    fn status_is_cut_to_the_window() {
        let f = frame(
            &art(3, 2),
            &tune(),
            "a-long-picture-name.jpg",
            (24, 8),
            true,
        );
        let rows = status_rows(&f);
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert!(row.chars().count() <= 24, "{row:?}");
        }
    }

    #[test]
    fn the_keys_shorten_to_fit_and_always_show_quit() {
        for cols in [100, 80, 40, 24] {
            let rows = status_rows(&frame(&art(3, 2), &tune(), "cat.jpg", (cols, 10), true));
            assert!(rows[1].contains("q quit"), "{cols}: {rows:?}");
            assert!(
                rows[1].chars().count() <= usize::from(cols),
                "{cols}: {rows:?}"
            );
        }
    }

    #[test]
    fn too_small_says_how_big() {
        let text = too_small((20, 6));
        assert!(
            text.contains("24 columns") && text.contains("8 rows"),
            "{text}"
        );
    }
}
