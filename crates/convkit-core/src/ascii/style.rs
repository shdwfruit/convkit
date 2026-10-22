//! The styles: which characters a cell gets, and in what colour.

use crate::{ConvError, ErrorCode, Result};

/// The classic ramp, from no ink to the most.
pub const RAMP: &str = " .:-=+*#%@";

/// Shades for blocks drawn without colour, from none to full.
pub const SHADES: [char; 5] = [' ', '░', '▒', '▓', '█'];

/// How cells are drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Style {
    /// `RAMP` by brightness, with `| / \ - _` on clear edges.
    Ascii,
    /// Half blocks, two colours a cell; `SHADES` without colour.
    Blocks,
    /// Eight dots a cell, dithered.
    Braille,
    /// The user's own characters, from least ink to most.
    Ramp(Vec<char>),
}

const NAMES: [&str; 3] = ["ascii", "blocks", "braille"];

impl Style {
    /// `--chars`: a style's name, or a ramp of two or more characters.
    /// Four or more lowercase letters that aren't a name are taken for a
    /// mistyped name rather than a ramp.
    pub fn parse(s: &str) -> Result<Style> {
        match s {
            "ascii" => return Ok(Style::Ascii),
            "blocks" => return Ok(Style::Blocks),
            "braille" => return Ok(Style::Braille),
            _ => {}
        }
        let invalid = |m: String| ConvError::new(ErrorCode::InvalidInvocation, m);
        if s.len() >= 4 && s.bytes().all(|b| b.is_ascii_lowercase()) {
            let near = NAMES
                .iter()
                .min_by_key(|n| strsim::levenshtein(n, s))
                .expect("three names");
            return Err(invalid(format!(
                "unknown style \"{s}\"; did you mean {near}? The styles are ascii, blocks and braille, or give your own characters from least ink to most, like \" .:oO@\""
            )));
        }
        let chars: Vec<char> = s.chars().collect();
        if let Some(c) = chars.iter().find(|c| c.is_control()) {
            return Err(invalid(format!(
                "--chars can't hold {c:?}; give printable characters from least ink to most, like \" .:oO@\""
            )));
        }
        if chars.len() < 2 {
            return Err(invalid(
                "a ramp needs at least two characters, from least ink to most, like \" .:oO@\""
                    .to_string(),
            ));
        }
        Ok(Style::Ramp(chars))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Style::Ascii => "ascii",
            Style::Blocks => "blocks",
            Style::Braille => "braille",
            Style::Ramp(_) => "ramp",
        }
    }
}

/// What the art will be shown on (`--bg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bg {
    Dark,
    Light,
}

impl Bg {
    /// The page colour for a PNG or web page.
    pub fn page(self) -> [u8; 3] {
        match self {
            Bg::Dark => [0x0c, 0x0c, 0x0e],
            Bg::Light => [0xff, 0xff, 0xff],
        }
    }

    /// The colour of characters that have none of their own.
    pub fn text(self) -> [u8; 3] {
        match self {
            Bg::Dark => [0xe4, 0xe4, 0xe7],
            Bg::Light => [0x18, 0x18, 0x1b],
        }
    }

    /// How much ink brightness `l` needs: on a dark page, light parts are
    /// the dense ones; on a light page, dark parts.
    pub fn ink(self, l: f32) -> f32 {
        match self {
            Bg::Dark => l,
            Bg::Light => 1.0 - l,
        }
    }
}

/// A sample counts towards an edge above this share of the picture's
/// 99th-percentile gradient...
const STRONG: f32 = 0.45;
/// ...and above this gradient, in brightness per cell width, so a smooth
/// gradient across the whole picture is never an edge.
const FLOOR: f32 = 2.0;
/// The share of a cell's strong samples that must point one way.
const DOMINANCE: f32 = 0.6;
/// A cell keeps its edge while it is at least this share of the
/// strongest cell beside it across the line.
const KEEP: f32 = 0.6;
/// The blur before the gradient, in samples.
const SIGMA: f32 = 1.2;

fn blur(v: &[f32], w: usize, h: usize) -> Vec<f32> {
    let radius = (SIGMA * 3.0).ceil() as isize;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|i| (-(i * i) as f32 / (2.0 * SIGMA * SIGMA)).exp())
        .collect();
    let total: f32 = kernel.iter().sum();
    let pass = |src: &[f32], step: usize, len: usize, lines: usize, across: usize| {
        let mut out = vec![0.0; src.len()];
        for line in 0..lines {
            for i in 0..len {
                let mut acc = 0.0;
                for (k, wgt) in kernel.iter().enumerate() {
                    let j = (i as isize + k as isize - radius).clamp(0, len as isize - 1) as usize;
                    acc += wgt * src[line * across + j * step];
                }
                out[line * across + i * step] = acc / total;
            }
        }
        out
    };
    let rows = pass(v, 1, w, h, w);
    pass(&rows, w, h, w, 1)
}

/// The edge character for each cell of a `w` x `h` brightness grid of
/// `sx` x `sy` samples a cell, where it holds one clear, straight edge.
pub fn edges(lum: &[f32], w: u32, h: u32, sx: u32, sy: u32, cell_aspect: f32) -> Vec<Option<char>> {
    let (w, h) = (w as usize, h as usize);
    let (sx, sy) = (sx as usize, sy as usize);
    let (cols, rows) = (w / sx, h / sy);
    let b = blur(lum, w, h);
    let at = |x: isize, y: isize| {
        b[y.clamp(0, h as isize - 1) as usize * w + x.clamp(0, w as isize - 1) as usize]
    };
    let mut mag = vec![0f32; w * h];
    let mut ang = vec![0f32; w * h];
    for y in 0..h as isize {
        for x in 0..w as isize {
            let gx = (at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x - 1, y) + at(x - 1, y + 1));
            let gy = (at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x, y - 1) + at(x + 1, y - 1));
            // Samples are not square in the picture: scale both to
            // distances in cell widths.
            let (gx, gy) = (gx * sx as f32, gy * sy as f32 / cell_aspect);
            let i = y as usize * w + x as usize;
            mag[i] = gx.hypot(gy);
            ang[i] = gy.atan2(gx).to_degrees().rem_euclid(180.0);
        }
    }
    let mut sorted = mag.clone();
    sorted.sort_by(f32::total_cmp);
    let p99 = sorted[((sorted.len() - 1) as f32 * 0.99).round() as usize];
    let strong = (STRONG * p99).max(FLOOR);
    // A thin edge across a cell covers about one side's worth of samples:
    // ask for half of a side's average, so horizontal edges (across the
    // short side) count as well as vertical ones.
    let need = (sx + sy).div_ceil(2);
    // How strong each cell's edge samples are in all, and its edge.
    let mut strength = vec![0f32; cols * rows];
    let mut found = vec![None; cols * rows];
    // Whether a cell's edge sits in its lower part, for `_` over `-`.
    let mut lower = vec![false; cols * rows];
    for cy in 0..rows {
        for cx in 0..cols {
            let mut bins = [0usize; 4];
            let (mut n, mut row_sum) = (0usize, 0usize);
            for dy in 0..sy {
                for dx in 0..sx {
                    let i = (cy * sy + dy) * w + cx * sx + dx;
                    if mag[i] <= strong {
                        continue;
                    }
                    strength[cy * cols + cx] += mag[i];
                    let a = ang[i];
                    let bin = if !(22.5..157.5).contains(&a) {
                        0
                    } else if a < 67.5 {
                        1
                    } else if a < 112.5 {
                        2
                    } else {
                        3
                    };
                    bins[bin] += 1;
                    n += 1;
                    row_sum += dy;
                }
            }
            if n < need {
                continue;
            }
            let (bin, &count) = bins
                .iter()
                .enumerate()
                .max_by_key(|(_, &c)| c)
                .expect("four bins");
            if (count as f32) < DOMINANCE * n as f32 {
                continue;
            }
            found[cy * cols + cx] = Some(bin);
            lower[cy * cols + cx] = row_sum as f32 / n as f32 >= 0.6 * sy as f32;
        }
    }
    // An edge spreads over the cells either side of it, more so with a
    // thick outline, which has one on each side: a cell gives its edge up
    // to a much stronger one across the line, so an outline is drawn one
    // character wide. Stripes, edges side by side and as strong, all keep
    // theirs.
    let at = |x: isize, y: isize| {
        if (0..cols as isize).contains(&x) && (0..rows as isize).contains(&y) {
            strength[y as usize * cols + x as usize]
        } else {
            0.0
        }
    };
    let mut out = vec![None; cols * rows];
    for cy in 0..rows {
        for cx in 0..cols {
            let i = cy * cols + cx;
            let Some(bin) = found[i] else { continue };
            // A step across the line: sideways for `|`, up or down for
            // `-`, and the diagonal crossing `/` or `\`.
            let (ox, oy) = [(1, 0), (1, 1), (0, 1), (1, -1)][bin];
            let (x, y) = (cx as isize, cy as isize);
            let beside = at(x + ox, y + oy).max(at(x - ox, y - oy));
            if strength[i] < KEEP * beside {
                continue;
            }
            out[i] = Some(match bin {
                0 => '|',
                1 => '/',
                2 if lower[i] => '_',
                2 => '-',
                _ => '\\',
            });
        }
    }
    out
}
