//! The tuner's screen: the art on the page it is made for, and two
//! status rows at the bottom. Built as one string, written in one go.

use convkit_core::ascii::{self, Art, Bg, Fit};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::tune::{Tune, LEAST_WIDTH};
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
/// smaller than `LEAST`. Never under `LEAST_WIDTH`, so a very tall
/// picture can still be taller than the window.
pub fn max_width(pic: (u32, u32), window: (u16, u16), cell_aspect: f32) -> Option<u32> {
    let (cols, rows) = window;
    if cols < LEAST.0 || rows < LEAST.1 {
        return None;
    }
    let room = u32::from(rows - STATUS_ROWS);
    let rows_at = |w: u32| ascii::layout(pic.0, pic.1, Fit::Width(w), cell_aspect).1;
    let (mut width, _, _) =
        ascii::layout(pic.0, pic.1, Fit::Box(u32::from(cols), room), cell_aspect);
    // The box's columns are rounded, which can give a row more than fits.
    while width > LEAST_WIDTH && rows_at(width) > room {
        width -= 1;
    }
    Some(width.max(LEAST_WIDTH))
}

/// `text` in at most `cols` columns of the screen: whole when it fits,
/// otherwise its start and its end with "…" between, so a question keeps
/// its "y/n" and the settings their last value.
fn fit(text: &str, cols: usize) -> String {
    if text.width() <= cols {
        return text.to_string();
    }
    let Some(room) = cols.checked_sub(1) else {
        return String::new();
    };
    let take = |chars: &mut dyn Iterator<Item = char>, budget: usize| {
        let mut used = 0;
        let mut kept = Vec::new();
        for c in chars {
            used += c.width().unwrap_or(0);
            if used > budget {
                break;
            }
            kept.push(c);
        }
        kept
    };
    let head = take(&mut text.chars(), room - room / 2);
    let mut tail = take(&mut text.chars().rev(), room / 2);
    tail.reverse();
    head.into_iter()
        .chain(std::iter::once('…'))
        .chain(tail)
        .collect()
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
    // Each row is cleared before it is written, never after: after a row
    // as wide as the window the cursor waits on its last column, and an
    // erase there would take that column in xterm-style terminals.
    let mut drawn = 0;
    for (r, line) in painted.lines().take(room).enumerate() {
        out.push_str(&format!("\x1b[{};1H\x1b[2K{line}\x1b[0m", r + 1));
        drawn = r + 1;
    }
    // Below the art, from the start of the next row.
    out.push_str(&format!("\x1b[{};1H\x1b[J", drawn + 1));
    // Art taller than the window, from a very tall picture, says so.
    let cut = if art.rows as usize > room {
        format!(" (top {room} of {} rows)", art.rows)
    } else {
        String::new()
    };
    let settings = format!(
        "{name}  {}x{}{cut}  {}  {}  colour {}  contrast {}  brightness {}",
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
        .find(|k| k.width() <= usize::from(cols))
        .unwrap_or(&KEYS[2]);
    let below = tune.message.as_deref().unwrap_or(keys);
    for (k, text) in [settings.as_str(), below].iter().enumerate() {
        let row = rows - STATUS_ROWS + 1 + k as u16;
        out.push_str(&format!(
            "\x1b[{row};1H\x1b[2K{}",
            fit(text, usize::from(cols))
        ));
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
    use unicode_width::UnicodeWidthStr;

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

    /// The text of the two status rows: what follows the last two
    /// `\x1b[2K`.
    fn status_rows(frame: &str) -> Vec<String> {
        let parts: Vec<&str> = frame.split("\x1b[2K").collect();
        parts[parts.len().saturating_sub(2)..]
            .iter()
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
    fn rows_are_cleared_before_they_are_written() {
        // After a row as wide as the window the cursor waits on its last
        // column, and an erase there takes that column in xterm-style
        // terminals (Ghostty among them): clear first, never after.
        let f = frame(&art(40, 2), &tune(), "cat.jpg", (40, 10), true);
        assert!(f.starts_with("\x1b[1;1H\x1b[2K"), "{f:?}");
        assert!(f.contains("\x1b[2;1H\x1b[2K"), "{f:?}");
        assert!(!f.contains("\x1b[K"), "{f:?}");
        assert!(f.contains("\x1b[3;1H\x1b[J"), "{f:?}");
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
    fn status_fits_by_screen_width() {
        // Each of these characters takes two columns.
        let name = "猫".repeat(20);
        let rows = status_rows(&frame(&art(3, 2), &tune(), &name, (40, 12), true));
        for row in rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 40, "{row:?}");
        }
    }

    #[test]
    fn a_long_message_keeps_its_end() {
        // The question's y/n stays when the name is too long to fit.
        let mut t = tune();
        t.message = Some(format!("{}.txt exists; replace it? y/n", "x".repeat(80)));
        let rows = status_rows(&frame(&art(3, 2), &t, "cat.jpg", (40, 12), true));
        assert!(
            rows[1].ends_with("y/n") && rows[1].contains('…'),
            "{rows:?}"
        );
        assert!(UnicodeWidthStr::width(rows[1].as_str()) <= 40, "{rows:?}");
    }

    #[test]
    fn max_width_leaves_no_row_out() {
        // Rounding the box's columns gave a tall picture one row too many.
        let most = max_width((100, 300), (80, 24), 2.0).unwrap();
        let (_, rows, _) = ascii::layout(100, 300, Fit::Width(most), 2.0);
        assert!(rows <= 22, "{most} columns make {rows} rows");
    }

    #[test]
    fn art_taller_than_the_window_says_so() {
        let f = frame(&art(8, 34), &tune(), "cat.jpg", (80, 24), true);
        let rows = status_rows(&f);
        assert!(rows[0].contains("top 22 of 34 rows"), "{rows:?}");
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
