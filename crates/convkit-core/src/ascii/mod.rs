//! ASCII art from pictures (`conv ascii`): the picture read as RGBA,
//! sampled onto a grid of character cells, and drawn as text, a web page
//! or a PNG. Nothing here prints; the `conv` binary writes the results.

pub mod grid;
pub mod image;

pub use image::{decode_png, from_svg, svg_has_text, Image};

/// The longest side a picture is read at. Enough samples for 400 columns
/// of braille (800 dots); more would only slow the read.
pub const READ_MAX_SIDE: u32 = 1600;

/// The most columns or rows `conv ascii` draws (`--width`/`--height` go
/// up to this too). A PNG of 400x400 cells is already 7200x14400 pixels.
pub const MAX_SIDE_CELLS: u32 = 400;

/// Columns when nothing else says how wide: files and pipes.
pub const DEFAULT_COLS: u32 = 100;

/// How big to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// This many columns, rows by the picture's shape.
    Width(u32),
    /// This many rows, columns by the picture's shape.
    Height(u32),
    /// As big as fits inside this many columns and rows.
    Box(u32, u32),
}

/// Columns and rows for a `pic_w` x `pic_h` picture in cells
/// `cell_aspect` times taller than wide, and a note when the size asked
/// for had to shrink to stay within `MAX_SIDE_CELLS`.
pub fn layout(pic_w: u32, pic_h: u32, fit: Fit, cell_aspect: f32) -> (u32, u32, Option<String>) {
    // Rows for a width, and the inverse.
    let shape = pic_h as f64 / pic_w.max(1) as f64 / cell_aspect as f64;
    let rows_for = |cols: u32| ((cols as f64 * shape).round() as u32).max(1);
    let cols_for = |rows: u32| ((rows as f64 / shape).round() as u32).max(1);
    let (cols, rows) = match fit {
        Fit::Width(c) => (c.max(1), rows_for(c.max(1))),
        Fit::Height(r) => (cols_for(r.max(1)), r.max(1)),
        Fit::Box(c, r) => {
            let (c, r) = (c.max(1), r.max(1));
            if rows_for(c) <= r {
                (c, rows_for(c))
            } else {
                (cols_for(r).min(c), r)
            }
        }
    };
    let plural = |n: u32, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if rows > MAX_SIDE_CELLS {
        let c2 = cols_for(MAX_SIDE_CELLS).min(cols);
        let note = format!(
            "{} would need {}; drew {} so it stays within {MAX_SIDE_CELLS} rows",
            plural(cols, "column"),
            plural(rows, "row"),
            plural(c2, "column")
        );
        return (c2, MAX_SIDE_CELLS, Some(note));
    }
    if cols > MAX_SIDE_CELLS {
        let r2 = rows_for(MAX_SIDE_CELLS).min(rows);
        let note = format!(
            "{} would need {}; drew {} so it stays within {MAX_SIDE_CELLS} columns",
            plural(rows, "row"),
            plural(cols, "column"),
            plural(r2, "row")
        );
        return (MAX_SIDE_CELLS, r2, Some(note));
    }
    (cols, rows, None)
}

/// A test picture, `f(x, y)` giving each pixel's RGBA.
#[cfg(test)]
pub(crate) fn pic(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Image {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&f(x, y));
        }
    }
    Image {
        width: w,
        height: h,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_follows_the_picture_and_the_cell() {
        assert_eq!(layout(400, 300, Fit::Width(100), 2.0), (100, 38, None));
        assert_eq!(layout(400, 300, Fit::Height(30), 2.0), (80, 30, None));
        assert_eq!(layout(400, 300, Fit::Box(80, 20), 2.0), (53, 20, None));
    }

    #[test]
    fn layout_caps_rows_and_says_so() {
        let (cols, rows, note) = layout(1, 1000, Fit::Width(400), 2.0);
        assert_eq!(rows, 400);
        assert!(cols >= 1);
        assert!(note.unwrap().contains("400 rows"));
        let (cols, _, note) = layout(1000, 1, Fit::Height(400), 2.0);
        assert_eq!(cols, 400);
        assert!(note.unwrap().contains("400 columns"));
    }

    #[test]
    fn layout_never_returns_zero() {
        let (cols, rows, _) = layout(1000, 1, Fit::Width(1), 2.0);
        assert!(cols >= 1 && rows >= 1);
        let (cols, rows, _) = layout(1, 1000, Fit::Height(1), 2.0);
        assert!(cols >= 1 && rows >= 1);
    }
}
