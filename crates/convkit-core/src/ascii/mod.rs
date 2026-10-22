//! ASCII art from pictures (`conv ascii`): the picture read as RGBA,
//! sampled onto a grid of character cells, and drawn as text, a web page
//! or a PNG. Nothing here prints; the `conv` binary writes the results.

pub mod art;
pub mod grid;
pub mod html;
pub mod image;
pub mod style;

pub use art::{Art, Cell};
pub use image::{decode_png, from_svg, svg_has_text, Image};
pub use style::{Bg, Style};

use grid::{adjust, colour_only, levels, luminance, Grid};
use style::{edges, RAMP, SHADES};

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

/// How to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub style: Style,
    pub bg: Bg,
    /// Keep the picture's colours.
    pub colour: bool,
    /// `--contrast` and `--brightness`, -100 to 100.
    pub contrast: i32,
    pub brightness: i32,
    /// How many times taller than wide a cell is: the value `layout` was
    /// given.
    pub cell_aspect: f32,
}

/// The art, and notes about it for the user.
#[derive(Debug, Clone)]
pub struct Made {
    pub art: Art,
    pub notes: Vec<String>,
}

const COLOUR_ONLY: &str = "this picture's shapes differ mostly in colour, not brightness, so they may not show; try --color or --chars blocks";

/// The empty braille character: no dots, but as wide as the others where
/// a font is not monospaced.
const NO_DOTS: char = '\u{2800}';

/// Draws `img` as `cols` x `rows` cells.
pub fn make(img: &Image, cols: u32, rows: u32, opts: &Options) -> Made {
    let (cols, rows) = (cols.max(1), rows.max(1));
    let (sx, sy) = match opts.style {
        Style::Ascii | Style::Ramp(_) => {
            (5, ((5.0 * opts.cell_aspect).round() as u32).clamp(5, 15))
        }
        Style::Braille => (2, 4),
        Style::Blocks => (1, 2),
    };
    let grid = grid::sample(img, cols * sx, rows * sy, opts.bg.page());
    let shape = Shape { cols, rows, sx, sy };
    let cells = if opts.style == Style::Blocks && opts.colour {
        coloured_blocks(&grid, &shape, opts)
    } else {
        let mut lum: Vec<f32> = grid.rgb.iter().map(|&c| luminance(c)).collect();
        levels(&mut lum, &grid.alpha);
        for v in &mut lum {
            *v = adjust(*v, opts.contrast, opts.brightness);
        }
        let ink: Vec<f32> = lum.iter().map(|&l| opts.bg.ink(l)).collect();
        let mut cells = match &opts.style {
            Style::Ascii => {
                let ramp: Vec<char> = RAMP.chars().collect();
                let marks = edges(&lum, cols * sx, rows * sy, sx, sy, opts.cell_aspect);
                let mut cells = ramp_cells(&ink, &shape, &ramp);
                for (cell, mark) in cells.iter_mut().zip(marks) {
                    if let Some(ch) = mark {
                        cell.ch = ch;
                    }
                }
                cells
            }
            Style::Ramp(ramp) => ramp_cells(&ink, &shape, ramp),
            Style::Blocks => ramp_cells(&ink, &shape, &SHADES),
            Style::Braille => braille_cells(ink, &shape),
        };
        if opts.colour {
            colour_cells(&mut cells, &grid, &shape, opts.bg);
        }
        cells
    };
    let mut notes = Vec::new();
    if !opts.colour && colour_only(&grid) {
        notes.push(COLOUR_ONLY.to_string());
    }
    Made {
        art: Art { cols, rows, cells },
        notes,
    }
}

/// The grid's cells and samples a cell.
struct Shape {
    cols: u32,
    rows: u32,
    sx: u32,
    sy: u32,
}

impl Shape {
    /// The sample indices in cell (`cx`, `cy`).
    fn samples(&self, cx: u32, cy: u32) -> impl Iterator<Item = usize> + '_ {
        let w = (self.cols * self.sx) as usize;
        (0..self.sy).flat_map(move |dy| {
            (0..self.sx)
                .map(move |dx| ((cy * self.sy + dy) as usize) * w + (cx * self.sx + dx) as usize)
        })
    }

    fn cells(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        (0..self.rows).flat_map(move |cy| (0..self.cols).map(move |cx| (cx, cy)))
    }
}

fn plain(ch: char) -> Cell {
    Cell {
        ch,
        fg: None,
        bg: None,
    }
}

/// Each cell's character from `ramp` by its mean ink.
fn ramp_cells(ink: &[f32], shape: &Shape, ramp: &[char]) -> Vec<Cell> {
    let n = ramp.len();
    shape
        .cells()
        .map(|(cx, cy)| {
            let (sum, count) = shape
                .samples(cx, cy)
                .fold((0.0, 0), |(s, c), i| (s + ink[i], c + 1));
            let mean: f32 = sum / count as f32;
            plain(ramp[((mean * n as f32) as usize).min(n - 1)])
        })
        .collect()
}

/// Dots by Floyd-Steinberg dithering of the ink, eight a cell.
fn braille_cells(mut ink: Vec<f32>, shape: &Shape) -> Vec<Cell> {
    let (w, h) = ((shape.cols * 2) as usize, (shape.rows * 4) as usize);
    let mut on = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let old = ink[y * w + x];
            let lit = old >= 0.5;
            on[y * w + x] = lit;
            let err = old - if lit { 1.0 } else { 0.0 };
            if x + 1 < w {
                ink[y * w + x + 1] += err * 7.0 / 16.0;
            }
            if y + 1 < h {
                if x > 0 {
                    ink[(y + 1) * w + x - 1] += err * 3.0 / 16.0;
                }
                ink[(y + 1) * w + x] += err * 5.0 / 16.0;
                if x + 1 < w {
                    ink[(y + 1) * w + x + 1] += err / 16.0;
                }
            }
        }
    }
    // Unicode's dot numbering: the left column top to bottom, then the
    // right, then the bottom pair.
    const BITS: [(usize, usize, u32); 8] = [
        (0, 0, 0x01),
        (0, 1, 0x02),
        (0, 2, 0x04),
        (1, 0, 0x08),
        (1, 1, 0x10),
        (1, 2, 0x20),
        (0, 3, 0x40),
        (1, 3, 0x80),
    ];
    shape
        .cells()
        .map(|(cx, cy)| {
            let bits = BITS
                .iter()
                .filter(|(dx, dy, _)| on[(cy as usize * 4 + dy) * w + cx as usize * 2 + dx])
                .fold(0, |b, (_, _, bit)| b | bit);
            plain(char::from_u32(0x2800 + bits).expect("a braille character"))
        })
        .collect()
}

/// Half blocks in the picture's own colours: the top sample in the
/// character's colour, the bottom in the background, and a transparent
/// sample left to whatever shows the art.
fn coloured_blocks(grid: &Grid, shape: &Shape, opts: &Options) -> Vec<Cell> {
    let colour = |i: usize| {
        (grid.alpha[i] >= 0.5).then(|| {
            grid.rgb[i].map(|c| (adjust(c, opts.contrast, opts.brightness) * 255.0).round() as u8)
        })
    };
    shape
        .cells()
        .map(|(cx, cy)| {
            let top = colour((cy as usize * 2) * shape.cols as usize + cx as usize);
            let bottom = colour((cy as usize * 2 + 1) * shape.cols as usize + cx as usize);
            match (top, bottom) {
                (Some(t), b) => Cell {
                    ch: '▀',
                    fg: Some(t),
                    bg: b,
                },
                (None, Some(b)) => Cell {
                    ch: '▄',
                    fg: Some(b),
                    bg: None,
                },
                (None, None) => plain(' '),
            }
        })
        .collect()
}

/// Colours each drawn character with its cell's colour, pushed to full
/// brightness on a dark page (the character's density already says how
/// bright it is) and kept darker on a light one, so it reads there.
fn colour_cells(cells: &mut [Cell], grid: &Grid, shape: &Shape, bg: Bg) {
    let target = match bg {
        Bg::Dark => 255.0,
        Bg::Light => 140.0,
    };
    for ((cx, cy), cell) in shape.cells().zip(cells.iter_mut()) {
        if cell.ch == ' ' || cell.ch == NO_DOTS {
            continue;
        }
        let (mut sum, mut n) = ([0f32; 3], 0);
        for i in shape.samples(cx, cy).filter(|&i| grid.alpha[i] >= 0.5) {
            for (total, c) in sum.iter_mut().zip(grid.rgb[i]) {
                *total += c;
            }
            n += 1;
        }
        if n == 0 {
            continue;
        }
        let mean = sum.map(|c| c / n as f32);
        let top = mean.iter().cloned().fold(0.0, f32::max);
        cell.fg = Some(if top > 0.0 {
            mean.map(|c| (c / top * target).round() as u8)
        } else {
            [0, 0, 0]
        });
    }
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

    fn opts(style: Style, bg: Bg, colour: bool) -> Options {
        Options {
            style,
            bg,
            colour,
            contrast: 0,
            brightness: 0,
            cell_aspect: 2.0,
        }
    }

    fn flat(rgba: [u8; 4]) -> Image {
        pic(40, 40, move |_, _| rgba)
    }

    fn chars(art: &Art) -> Vec<char> {
        art.cells.iter().map(|c| c.ch).collect()
    }

    fn row(art: &Art, r: u32) -> String {
        art.cells[(r * art.cols) as usize..((r + 1) * art.cols) as usize]
            .iter()
            .map(|c| c.ch)
            .collect()
    }

    fn col(art: &Art, c: u32) -> String {
        (0..art.rows)
            .map(|r| art.cells[(r * art.cols + c) as usize].ch)
            .collect()
    }

    #[test]
    fn style_parse_names_and_ramps() {
        assert_eq!(Style::parse("ascii").unwrap(), Style::Ascii);
        assert_eq!(Style::parse("blocks").unwrap(), Style::Blocks);
        assert_eq!(Style::parse("braille").unwrap(), Style::Braille);
        assert_eq!(
            Style::parse(" .oO@").unwrap(),
            Style::Ramp(" .oO@".chars().collect())
        );
        let e = Style::parse("brail").unwrap_err();
        assert!(e.message.contains("did you mean braille"), "{}", e.message);
        let e = Style::parse("x").unwrap_err();
        assert!(e.message.contains("at least two"), "{}", e.message);
        assert!(Style::parse("a\tb").is_err());
        assert_eq!(
            Style::parse("abc").unwrap(),
            Style::Ramp(vec!['a', 'b', 'c'])
        );
    }

    #[test]
    fn ascii_maps_brightness_on_a_dark_background() {
        let white = make(&flat([255; 4]), 10, 5, &opts(Style::Ascii, Bg::Dark, false)).art;
        assert!(chars(&white).iter().all(|&c| c == '@'));
        let black = make(
            &flat([0, 0, 0, 255]),
            10,
            5,
            &opts(Style::Ascii, Bg::Dark, false),
        )
        .art;
        assert_eq!(black.text(), "\n".repeat(5));
    }

    #[test]
    fn light_background_flips_the_ramp() {
        let white = make(
            &flat([255; 4]),
            10,
            5,
            &opts(Style::Ascii, Bg::Light, false),
        )
        .art;
        assert!(chars(&white).iter().all(|&c| c == ' '));
        let black = make(
            &flat([0, 0, 0, 255]),
            10,
            5,
            &opts(Style::Ascii, Bg::Light, false),
        )
        .art;
        assert!(chars(&black).iter().all(|&c| c == '@'));
    }

    #[test]
    fn ascii_draws_edges_the_right_way_round() {
        let draw = |bright: fn(u32, u32) -> bool| {
            let img = pic(200, 200, |x, y| {
                if bright(x, y) {
                    [255; 4]
                } else {
                    [0, 0, 0, 255]
                }
            });
            make(&img, 20, 10, &opts(Style::Ascii, Bg::Dark, false)).art
        };
        let most = |s: &str, set: &[char]| {
            s.chars().filter(|c| set.contains(c)).count() * 2 > s.chars().count()
        };

        let v = draw(|x, _| x > 100);
        assert!(
            most(&col(&v, 9), &['|']) || most(&col(&v, 10), &['|']),
            "{}",
            v.text()
        );

        let h = draw(|_, y| y > 100);
        assert!(
            most(&row(&h, 4), &['-', '_']) || most(&row(&h, 5), &['-', '_']),
            "{}",
            h.text()
        );

        let rising = draw(|x, y| x + y > 200);
        let t = rising.text();
        assert!(t.contains('/') && !t.contains('\\'), "{t}");

        let falling = draw(|x, y| x > y);
        let t = falling.text();
        assert!(t.contains('\\') && !t.contains('/'), "{t}");

        for img in [
            flat([128, 128, 128, 255]),
            pic(200, 200, |x, _| {
                let v = (x * 255 / 199) as u8;
                [v, v, v, 255]
            }),
        ] {
            let t = make(&img, 20, 10, &opts(Style::Ascii, Bg::Dark, false))
                .art
                .text();
            // `-` is also one of RAMP's tones, so only the edge-only
            // characters say an edge was drawn.
            assert!(!t.contains(['|', '/', '\\', '_']), "{t}");
        }
    }

    #[test]
    fn blocks_keep_colour_and_transparency() {
        let two = |top: [u8; 4], bottom: [u8; 4]| {
            let img = pic(1, 2, move |_, y| if y == 0 { top } else { bottom });
            let art = make(&img, 1, 1, &opts(Style::Blocks, Bg::Dark, true)).art;
            let c = art.cells[0];
            (c.ch, c.fg, c.bg)
        };
        let (red, blue, clear) = ([255, 0, 0, 255], [0, 0, 255, 255], [0, 0, 0, 0]);
        assert_eq!(two(red, blue), ('▀', Some([255, 0, 0]), Some([0, 0, 255])));
        assert_eq!(two(red, clear), ('▀', Some([255, 0, 0]), None));
        assert_eq!(two(clear, blue), ('▄', Some([0, 0, 255]), None));
        assert_eq!(two(clear, clear), (' ', None, None));
    }

    #[test]
    fn blocks_without_colour_use_shades() {
        let white = make(&flat([255; 4]), 4, 2, &opts(Style::Blocks, Bg::Dark, false)).art;
        assert!(chars(&white).iter().all(|&c| c == '█'));
        let black = make(
            &flat([0, 0, 0, 255]),
            4,
            2,
            &opts(Style::Blocks, Bg::Dark, false),
        )
        .art;
        assert!(chars(&black).iter().all(|&c| c == ' '));
    }

    #[test]
    fn braille_bits_follow_the_dots() {
        let one = pic(2, 4, |x, y| {
            if (x, y) == (0, 0) {
                [255; 4]
            } else {
                [0, 0, 0, 255]
            }
        });
        let o = opts(Style::Braille, Bg::Dark, false);
        assert_eq!(make(&one, 1, 1, &o).art.cells[0].ch, '\u{2801}');
        assert_eq!(make(&flat([255; 4]), 1, 1, &o).art.cells[0].ch, '\u{28FF}');
        assert_eq!(
            make(&flat([0, 0, 0, 255]), 1, 1, &o).art.cells[0].ch,
            '\u{2800}'
        );
    }

    #[test]
    fn ascii_text_is_seven_bit() {
        let img = pic(64, 64, |x, y| {
            [(x * 4) as u8, (y * 4) as u8, ((x ^ y) * 4) as u8, 255]
        });
        assert!(make(&img, 30, 15, &opts(Style::Ascii, Bg::Dark, false))
            .art
            .text()
            .is_ascii());
    }

    #[test]
    fn colour_cells_are_pushed_to_full_brightness() {
        let img = pic(40, 40, |x, _| {
            if x < 20 {
                [0, 0, 0, 255]
            } else {
                [128, 0, 0, 255]
            }
        });
        let art = make(&img, 10, 5, &opts(Style::Ascii, Bg::Dark, true)).art;
        let mut lit = 0;
        for r in 0..5 {
            for c in 0..10 {
                let cell = art.cells[(r * 10 + c) as usize];
                if cell.ch == ' ' {
                    assert_eq!(cell.fg, None);
                } else if c >= 5 {
                    assert_eq!(cell.fg, Some([255, 0, 0]));
                    lit += 1;
                }
            }
        }
        assert!(lit > 0, "{}", art.text());
    }

    fn red_green() -> Image {
        pic(40, 20, |x, _| {
            if x < 20 {
                [255, 0, 0, 255]
            } else {
                [0, 76, 0, 255]
            }
        })
    }

    #[test]
    fn a_colour_only_picture_gets_a_note() {
        let made = make(&red_green(), 20, 5, &opts(Style::Ascii, Bg::Dark, false));
        assert!(
            made.notes.iter().any(|n| n.contains("try --color")),
            "{:?}",
            made.notes
        );
    }

    #[test]
    fn no_note_with_colour() {
        let made = make(&red_green(), 20, 5, &opts(Style::Ascii, Bg::Dark, true));
        assert!(made.notes.is_empty(), "{:?}", made.notes);
    }

    #[test]
    fn make_survives_tiny_and_empty_pictures() {
        for style in [
            Style::Ascii,
            Style::Blocks,
            Style::Braille,
            Style::Ramp(vec![' ', '#']),
        ] {
            for colour in [false, true] {
                let o = opts(style.clone(), Bg::Dark, colour);
                make(&flat([9, 9, 9, 255]), 1, 1, &o);
                make(&pic(2, 2, |x, _| [(x * 200) as u8, 0, 0, 255]), 100, 50, &o);
                let clear = make(&pic(10, 10, |_, _| [0; 4]), 40, 20, &o).art;
                if !matches!(style, Style::Braille) {
                    assert_eq!(clear.text(), "\n".repeat(20));
                }
            }
        }
    }

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
