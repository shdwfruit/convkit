//! The art drawn as a PNG.
//!
//! ASCII characters come from a pre-drawn atlas of JetBrains Mono
//! (`glyphs.png`, made by `scripts/ascii-glyphs.py`; licence in
//! `glyphs-OFL.txt`), so drawing needs no font code. Blocks and braille
//! are drawn as shapes, crisp at any size and in no need of a font.

use std::sync::LazyLock;

use super::{decode_png, Art, Bg};
use crate::{ConvError, ErrorCode, Result};

/// A cell's width and height in the PNG: 100 columns are 1800 pixels.
pub const CELL_W: u32 = 18;
pub const CELL_H: u32 = 36;

/// `!` to `~`, one `CELL_W` x `CELL_H` cell each, in a row: how much ink
/// covers each pixel.
static ATLAS: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let img = decode_png(include_bytes!("glyphs.png")).expect("the glyph atlas is a PNG");
    assert_eq!(
        (img.width, img.height),
        (CELL_W * 94, CELL_H),
        "the glyph atlas's size"
    );
    img.rgba.chunks_exact(4).map(|p| p[0]).collect()
});

/// Whether the PNG can draw `c`.
pub fn drawable(c: char) -> bool {
    matches!(c, ' '..='~' | '░' | '▒' | '▓' | '█' | '▀' | '▄' | '\u{2800}'..='\u{28FF}')
}

struct Canvas {
    w: u32,
    px: Vec<u8>,
}

impl Canvas {
    /// Lays `c` over pixel (`x`, `y`) with coverage `a`, 0 to 1.
    fn blend(&mut self, x: u32, y: u32, c: [u8; 3], a: f32) {
        let i = ((y * self.w + x) * 3) as usize;
        for (under, over) in self.px[i..i + 3].iter_mut().zip(c) {
            let u = *under as f32;
            *under = (u + (over as f32 - u) * a).round() as u8;
        }
    }

    fn fill(&mut self, x0: u32, y0: u32, w: u32, h: u32, c: [u8; 3]) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                self.blend(x, y, c, 1.0);
            }
        }
    }
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [0, 1, 2].map(|k| (b[k] as f32 + (a[k] as f32 - b[k] as f32) * t).round() as u8)
}

/// Draws `art` on `bg`'s page colour, `CELL_W` x `CELL_H` pixels a cell.
pub fn png(art: &Art, bg: Bg) -> Result<Vec<u8>> {
    if let Some(c) = art.cells.iter().map(|c| c.ch).find(|&c| !drawable(c)) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("a .png can draw ASCII, block and braille characters; \"{c}\" isn't one"),
        ));
    }
    let (w, h) = (art.cols * CELL_W, art.rows * CELL_H);
    let page = bg.page();
    let mut canvas = Canvas {
        w,
        px: page
            .iter()
            .copied()
            .cycle()
            .take((w * h * 3) as usize)
            .collect(),
    };
    let atlas_w = CELL_W * 94;
    for (i, cell) in art.cells.iter().enumerate() {
        let (x0, y0) = (
            (i as u32 % art.cols) * CELL_W,
            (i as u32 / art.cols) * CELL_H,
        );
        let back = cell.bg.unwrap_or(page);
        if let Some(b) = cell.bg {
            canvas.fill(x0, y0, CELL_W, CELL_H, b);
        }
        let ink = cell.fg.unwrap_or(bg.text());
        match cell.ch {
            ' ' | '\u{2800}' => {}
            '█' => canvas.fill(x0, y0, CELL_W, CELL_H, ink),
            '▓' => canvas.fill(x0, y0, CELL_W, CELL_H, mix(ink, back, 0.75)),
            '▒' => canvas.fill(x0, y0, CELL_W, CELL_H, mix(ink, back, 0.5)),
            '░' => canvas.fill(x0, y0, CELL_W, CELL_H, mix(ink, back, 0.25)),
            '▀' => canvas.fill(x0, y0, CELL_W, CELL_H / 2, ink),
            '▄' => canvas.fill(x0, y0 + CELL_H / 2, CELL_W, CELL_H / 2, ink),
            c @ '\u{2801}'..='\u{28FF}' => dots(&mut canvas, x0, y0, c as u32 - 0x2800, ink),
            c => {
                let at = (c as u32 - '!' as u32) * CELL_W;
                for y in 0..CELL_H {
                    for x in 0..CELL_W {
                        let a = ATLAS[(y * atlas_w + at + x) as usize];
                        if a > 0 {
                            canvas.blend(x0 + x, y0 + y, ink, a as f32 / 255.0);
                        }
                    }
                }
            }
        }
    }
    encode(w, h, &canvas.px)
}

/// Braille dots, in Unicode's numbering: the left column top to bottom,
/// then the right, then the bottom pair.
fn dots(canvas: &mut Canvas, x0: u32, y0: u32, bits: u32, ink: [u8; 3]) {
    const SPOTS: [(f32, f32); 8] = [
        (0.3, 0.125),
        (0.3, 0.375),
        (0.3, 0.625),
        (0.7, 0.125),
        (0.7, 0.375),
        (0.7, 0.625),
        (0.3, 0.875),
        (0.7, 0.875),
    ];
    const R: f32 = 3.0;
    for (bit, (fx, fy)) in SPOTS.iter().enumerate() {
        if bits >> bit & 1 == 0 {
            continue;
        }
        let (cx, cy) = (fx * CELL_W as f32, fy * CELL_H as f32);
        let (lo_x, hi_x) = (
            (cx - R - 1.0).max(0.0) as u32,
            ((cx + R + 1.0) as u32).min(CELL_W - 1),
        );
        let (lo_y, hi_y) = (
            (cy - R - 1.0).max(0.0) as u32,
            ((cy + R + 1.0) as u32).min(CELL_H - 1),
        );
        for y in lo_y..=hi_y {
            for x in lo_x..=hi_x {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                let a = (R + 0.5 - d).clamp(0.0, 1.0);
                if a > 0.0 {
                    canvas.blend(x0 + x, y0 + y, ink, a);
                }
            }
        }
    }
}

fn encode(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    let failed = |e: ::png::EncodingError| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not write the PNG: {e}"),
        )
    };
    let mut out = Vec::new();
    let mut encoder = ::png::Encoder::new(&mut out, w, h);
    encoder.set_color(::png::ColorType::Rgb);
    encoder.set_depth(::png::BitDepth::Eight);
    encoder.set_compression(::png::Compression::High);
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(rgb).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascii::{decode_png, Cell};

    fn art(cols: u32, rows: u32, cell: Cell) -> Art {
        Art {
            cols,
            rows,
            cells: vec![cell; (cols * rows) as usize],
        }
    }

    fn plain(ch: char) -> Cell {
        Cell {
            ch,
            fg: None,
            bg: None,
        }
    }

    fn pixel(img: &crate::ascii::Image, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * img.width + x) * 4) as usize;
        [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2]]
    }

    #[test]
    fn png_is_cells_times_the_art() {
        let img = decode_png(&png(&art(3, 2, plain('#')), Bg::Dark).unwrap()).unwrap();
        assert_eq!((img.width, img.height), (54, 72));
    }

    #[test]
    fn png_blocks_are_exact() {
        let cell = Cell {
            ch: '▀',
            fg: Some([255, 0, 0]),
            bg: Some([0, 0, 255]),
        };
        let img = decode_png(&png(&art(1, 1, cell), Bg::Dark).unwrap()).unwrap();
        assert_eq!(pixel(&img, 9, 5), [255, 0, 0]);
        assert_eq!(pixel(&img, 9, 30), [0, 0, 255]);
        let lower = Cell {
            ch: '▄',
            fg: Some([0, 255, 0]),
            bg: None,
        };
        let img = decode_png(&png(&art(1, 1, lower), Bg::Light).unwrap()).unwrap();
        assert_eq!(pixel(&img, 9, 5), Bg::Light.page());
        assert_eq!(pixel(&img, 9, 30), [0, 255, 0]);
    }

    #[test]
    fn png_text_is_drawn() {
        let mut a = art(2, 1, plain('@'));
        a.cells[1] = plain(' ');
        let img = decode_png(&png(&a, Bg::Dark).unwrap()).unwrap();
        let page = Bg::Dark.page();
        let inked = (0..CELL_H).any(|y| (0..CELL_W).any(|x| pixel(&img, x, y) != page));
        assert!(inked, "the @ cell has ink");
        let blank = (0..CELL_H).all(|y| (CELL_W..2 * CELL_W).all(|x| pixel(&img, x, y) == page));
        assert!(blank, "the space cell is all page");
    }

    #[test]
    fn png_braille_dots_are_drawn() {
        let img = decode_png(&png(&art(1, 1, plain('\u{2801}')), Bg::Dark).unwrap()).unwrap();
        // The top-left dot's centre, and the bottom-right one's.
        assert_ne!(pixel(&img, 5, 4), Bg::Dark.page());
        assert_eq!(pixel(&img, 13, 31), Bg::Dark.page());
    }

    #[test]
    fn png_refuses_what_it_cannot_draw() {
        let e = png(&art(1, 1, plain('é')), Bg::Dark).unwrap_err();
        assert!(e.message.contains('é'), "{}", e.message);
        assert!(drawable('~') && drawable('▓') && drawable('\u{28FF}'));
        assert!(!drawable('é') && !drawable('\t'));
    }
}
