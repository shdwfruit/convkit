//! The main colours of a picture (`conv colors`), and the ways to write a
//! colour down: HEX, RGB, HSL, and code for CSS, SwiftUI and Tailwind.
//!
//! The palette is k-means in OKLab, where equal distances look like equal
//! differences, so a cluster is a colour a person would call one colour.
//! The pixels are first counted into a 32x32x32 grid of sRGB values, which
//! makes the work the same for a 48-megapixel photo as for a thumbnail, and
//! the clustering runs over the grid's cells weighted by their counts. It
//! starts from the most common colour, then repeatedly from the colour that
//! is both common and far from those chosen, so the result is the same on
//! every run. Clusters that end up closer than a visible difference are
//! merged, so a two-colour logo gives two colours, not six shades of them.

use std::fmt::Write as _;

use crate::ascii::Image;
use crate::{ConvError, ErrorCode, Result};

/// Colours in a palette unless asked otherwise.
pub const DEFAULT_COUNT: usize = 6;

/// The most colours a palette gives.
pub const MAX_COUNT: usize = 16;

/// Clusters closer than this in OKLab are one colour. A just-noticeable
/// difference is about 0.02; twice that keeps shades a person tells apart
/// and folds together the noise around one colour in a JPEG.
const MERGE_BELOW: f32 = 0.04;

/// Lloyd iterations at most; the grid's cells settle long before.
const MAX_ROUNDS: usize = 100;

/// A pixel at least this opaque counts; fainter ones are left out.
const OPAQUE: u8 = 128;

/// An sRGB colour, with its opacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Colour {
    pub fn rgb(r: u8, g: u8, b: u8) -> Colour {
        Colour { r, g, b, a: 255 }
    }

    /// `#2b4a6f`, or `#2b4a6f80` when it is not opaque.
    pub fn hex(&self) -> String {
        let mut s = format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b);
        if self.a < 255 {
            let _ = write!(s, "{:02x}", self.a);
        }
        s
    }

    /// `rgb(43, 74, 111)`, or `rgba(43, 74, 111, 0.5)`.
    pub fn rgb_css(&self) -> String {
        if self.a < 255 {
            format!(
                "rgba({}, {}, {}, {})",
                self.r,
                self.g,
                self.b,
                alpha(self.a)
            )
        } else {
            format!("rgb({}, {}, {})", self.r, self.g, self.b)
        }
    }

    /// Hue in degrees, saturation and lightness in percent.
    pub fn hsl(&self) -> [u16; 3] {
        let [r, g, b] = [self.r, self.g, self.b].map(|v| f32::from(v) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        if d == 0.0 {
            return [0, 0, (l * 100.0).round() as u16];
        }
        let s = d / (1.0 - (2.0 * l - 1.0).abs());
        let h = if max == r {
            60.0 * ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        [
            (h.round() as u16) % 360,
            (s * 100.0).round() as u16,
            (l * 100.0).round() as u16,
        ]
    }

    /// `hsl(213, 44%, 30%)`, or `hsla(213, 44%, 30%, 0.5)`.
    pub fn hsl_css(&self) -> String {
        let [h, s, l] = self.hsl();
        if self.a < 255 {
            format!("hsla({h}, {s}%, {l}%, {})", alpha(self.a))
        } else {
            format!("hsl({h}, {s}%, {l}%)")
        }
    }
}

/// An opacity as CSS writes it: 0 to 1, in as few digits as tell it apart.
fn alpha(a: u8) -> String {
    let s = format!("{:.3}", f32::from(a) / 255.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// One colour of a palette, and the share of the picture it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swatch {
    pub colour: Colour,
    /// 0 to 1 of the picture's opaque pixels.
    pub share: f32,
}

/// The colour at `(x, y)`, counted from the top-left, as the picture is
/// shown.
pub fn at(img: &Image, x: u32, y: u32) -> Result<Colour> {
    if x >= img.width || y >= img.height {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{x},{y} is outside the picture, which is {}x{} (0,0 to {},{})",
                img.width,
                img.height,
                img.width.saturating_sub(1),
                img.height.saturating_sub(1)
            ),
        ));
    }
    let i = ((y * img.width + x) * 4) as usize;
    let p = &img.rgba[i..i + 4];
    Ok(Colour {
        r: p[0],
        g: p[1],
        b: p[2],
        a: p[3],
    })
}

/// Up to `count` main colours of `img`, the most common first. Fewer when
/// the picture has fewer colours a person would tell apart.
pub fn palette(img: &Image, count: usize) -> Result<Vec<Swatch>> {
    let count = count.clamp(1, MAX_COUNT);
    let points = grid(img);
    let total: f64 = points.iter().map(|p| p.weight).sum();
    if total == 0.0 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "the picture is transparent everywhere, so it has no colours",
        ));
    }
    let mut centres = seeds(&points, count);
    lloyd(&points, &mut centres);
    while merge_closest(&mut centres) {}
    let owner = lloyd(&points, &mut centres);
    let colours = representatives(img, &points, &owner, centres.len());
    let mut swatches: Vec<Swatch> = centres
        .iter()
        .zip(colours)
        .filter(|(c, _)| c.weight > 0.0)
        .map(|(c, colour)| Swatch {
            colour: colour.unwrap_or_else(|| from_oklab(c.lab)),
            share: (c.weight / total) as f32,
        })
        .collect();
    swatches.sort_by(|a, b| b.share.total_cmp(&a.share));
    Ok(swatches)
}

/// The colour each cluster is shown as: the most common exact colour in
/// its most common cell. A colour that is really in the picture, exactly
/// so in a flat design, where a cluster's mean would be a blend of its
/// neighbours that appears nowhere.
fn representatives(
    img: &Image,
    points: &[Point],
    owner: &[usize],
    clusters: usize,
) -> Vec<Option<Colour>> {
    let mut best: Vec<Option<&Point>> = vec![None; clusters];
    for (p, &o) in points.iter().zip(owner) {
        if best[o].is_none_or(|b| p.weight > b.weight) {
            best[o] = Some(p);
        }
    }
    let mut slot = vec![usize::MAX; 1 << 15];
    for (i, b) in best.iter().enumerate() {
        if let Some(b) = b {
            slot[b.key] = i;
        }
    }
    let mut counts: Vec<std::collections::HashMap<[u8; 3], u64>> =
        vec![Default::default(); clusters];
    for p in img.rgba.as_chunks::<4>().0 {
        if p[3] < OPAQUE {
            continue;
        }
        let i = slot[cell_of(p)];
        if i != usize::MAX {
            *counts[i].entry([p[0], p[1], p[2]]).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|c| {
            // Most common first, then the lowest value, so a tie always
            // gives the same answer.
            c.into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
                .map(|(rgb, _)| Colour::rgb(rgb[0], rgb[1], rgb[2]))
        })
        .collect()
}

/// The grid cell an RGBA pixel falls in: 5 bits a channel.
fn cell_of(p: &[u8]) -> usize {
    (usize::from(p[0] >> 3) << 10) | (usize::from(p[1] >> 3) << 5) | usize::from(p[2] >> 3)
}

/// A colour in OKLab with the weight of the pixels behind it.
#[derive(Debug, Clone, Copy)]
struct Point {
    lab: [f32; 3],
    weight: f64,
    /// The grid cell it came from.
    key: usize,
}

/// The opaque pixels counted into a 32x32x32 grid, each cell as the mean
/// of its pixels.
fn grid(img: &Image) -> Vec<Point> {
    let mut cells = vec![(0u64, [0u64; 3]); 1 << 15];
    for p in img.rgba.as_chunks::<4>().0 {
        if p[3] < OPAQUE {
            continue;
        }
        let cell = &mut cells[cell_of(p)];
        cell.0 += 1;
        for (sum, &v) in cell.1.iter_mut().zip(p) {
            *sum += u64::from(v);
        }
    }
    cells
        .into_iter()
        .enumerate()
        .filter(|(_, (n, _))| *n > 0)
        .map(|(key, (n, sum))| Point {
            lab: oklab(sum.map(|s| s as f32 / n as f32)),
            weight: n as f64,
            key,
        })
        .collect()
}

/// The first centres: the most common colour, then each time the one
/// whose weight times its squared distance to the nearest centre is
/// largest. Deterministic, unlike k-means++'s draw.
fn seeds(points: &[Point], count: usize) -> Vec<Point> {
    let first = points
        .iter()
        .copied()
        .max_by(|a, b| a.weight.total_cmp(&b.weight))
        .expect("at least one point");
    let mut centres = vec![first];
    let mut nearest: Vec<f32> = points.iter().map(|p| dist2(p.lab, first.lab)).collect();
    while centres.len() < count {
        let Some((i, score)) = points
            .iter()
            .zip(&nearest)
            .map(|(p, d)| p.weight * f64::from(*d))
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            break;
        };
        if score <= 0.0 {
            break;
        }
        let chosen = points[i];
        centres.push(chosen);
        for (n, p) in nearest.iter_mut().zip(points) {
            *n = n.min(dist2(p.lab, chosen.lab));
        }
    }
    centres
}

/// Moves each centre to the weighted mean of the points nearest it until
/// nothing changes, and gives each point's centre. A centre left with no
/// points keeps weight 0.
fn lloyd(points: &[Point], centres: &mut [Point]) -> Vec<usize> {
    let mut owner = vec![usize::MAX; points.len()];
    for _ in 0..MAX_ROUNDS {
        let mut changed = false;
        for (o, p) in owner.iter_mut().zip(points) {
            let best = (0..centres.len())
                .min_by(|&a, &b| {
                    dist2(p.lab, centres[a].lab).total_cmp(&dist2(p.lab, centres[b].lab))
                })
                .expect("at least one centre");
            if *o != best {
                *o = best;
                changed = true;
            }
        }
        let mut sums = vec![([0f64; 3], 0f64); centres.len()];
        for (&o, p) in owner.iter().zip(points) {
            for (sum, v) in sums[o].0.iter_mut().zip(p.lab) {
                *sum += f64::from(v) * p.weight;
            }
            sums[o].1 += p.weight;
        }
        for (centre, (sum, weight)) in centres.iter_mut().zip(sums) {
            centre.weight = weight;
            if weight > 0.0 {
                centre.lab = sum.map(|s| (s / weight) as f32);
            }
        }
        if !changed {
            break;
        }
    }
    owner
}

/// Merges the two closest centres if they are closer than `MERGE_BELOW`,
/// and says whether it did.
fn merge_closest(centres: &mut Vec<Point>) -> bool {
    centres.retain(|c| c.weight > 0.0);
    let mut best: Option<(usize, usize, f32)> = None;
    for i in 0..centres.len() {
        for j in i + 1..centres.len() {
            let d = dist2(centres[i].lab, centres[j].lab);
            if best.is_none_or(|b| d < b.2) {
                best = Some((i, j, d));
            }
        }
    }
    let Some((i, j, d)) = best else {
        return false;
    };
    if d >= MERGE_BELOW * MERGE_BELOW {
        return false;
    }
    let (a, b) = (centres[i], centres[j]);
    let weight = a.weight + b.weight;
    let mut lab = [0f32; 3];
    for (c, v) in lab.iter_mut().enumerate() {
        *v = ((f64::from(a.lab[c]) * a.weight + f64::from(b.lab[c]) * b.weight) / weight) as f32;
    }
    centres[i] = Point { lab, weight, ..a };
    centres.remove(j);
    true
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

fn to_linear(v: f32) -> f32 {
    let v = v / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// A linear channel to sRGB, 0 to 1.
fn from_linear(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0 to 255 a channel) to OKLab, by Björn Ottosson's matrices.
pub(crate) fn oklab(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(to_linear);
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// OKLab back to the nearest sRGB colour.
pub(crate) fn from_oklab(lab: [f32; 3]) -> Colour {
    let [r, g, b] = oklab_to_srgb_f32(lab).map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8);
    Colour::rgb(r, g, b)
}

/// OKLab to sRGB, 0 to 1 a channel, clamped to what sRGB can show.
pub(crate) fn oklab_to_srgb_f32(lab: [f32; 3]) -> [f32; 3] {
    let [l, a, b] = lab;
    let l_ = (l + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m_ = (l - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s_ = (l - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    let r = 4.076_741_7 * l_ - 3.307_711_6 * m_ + 0.230_969_94 * s_;
    let g = -1.268_438 * l_ + 2.609_757_4 * m_ - 0.341_319_38 * s_;
    let bl = -0.004_196_086_3 * l_ - 0.703_418_6 * m_ + 1.707_614_7 * s_;
    [r, g, bl].map(from_linear)
}

/// A language to write colours in for `--code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// CSS custom properties on `:root`.
    Css,
    /// A SwiftUI `Color` extension.
    Swiftui,
    /// Tailwind CSS 4's `@theme` block.
    Tailwind,
    /// The `colors` of a Tailwind CSS 3 `tailwind.config.js`.
    Tailwind3,
}

impl Code {
    pub const NAMES: [&'static str; 4] = ["css", "swiftui", "tailwind", "tailwind3"];

    pub fn parse(s: &str) -> std::result::Result<Code, String> {
        match s.to_ascii_lowercase().as_str() {
            "css" => Ok(Code::Css),
            "swiftui" | "swift" => Ok(Code::Swiftui),
            "tailwind" | "tailwind4" => Ok(Code::Tailwind),
            "tailwind3" => Ok(Code::Tailwind3),
            _ => Err(format!(
                "expected one of {}, got {s:?}",
                Code::NAMES.join(", ")
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Code::Css => "css",
            Code::Swiftui => "swiftui",
            Code::Tailwind => "tailwind",
            Code::Tailwind3 => "tailwind3",
        }
    }
}

/// `colours` as code, named palette-1, palette-2 ... in order, each with
/// its share as a comment when it has one.
pub fn code(colours: &[(Colour, Option<f32>)], kind: Code) -> String {
    let comment = |share: Option<f32>, open: &str, close: &str| {
        share
            .map_or(String::new(), |s| format!(" {open} {} {close}", percent(s)))
            .trim_end()
            .to_string()
    };
    let mut s = String::new();
    match kind {
        Code::Css | Code::Tailwind => {
            let (open, prefix) = if kind == Code::Css {
                (":root {", "--palette")
            } else {
                ("@theme {", "--color-palette")
            };
            s.push_str(open);
            s.push('\n');
            for (i, (c, share)) in colours.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "  {prefix}-{}: {};{}",
                    i + 1,
                    c.hex(),
                    comment(*share, "/*", "*/")
                );
            }
            s.push_str("}\n");
        }
        Code::Swiftui => {
            s.push_str("import SwiftUI\n\nextension Color {\n");
            for (i, (c, share)) in colours.iter().enumerate() {
                let f = |v: u8| format!("{:.3}", f32::from(v) / 255.0);
                let opacity = if c.a < 255 {
                    format!(", opacity: {}", f(c.a))
                } else {
                    String::new()
                };
                let _ = writeln!(
                    s,
                    "    static let palette{} = Color(red: {}, green: {}, blue: {}{opacity}){}",
                    i + 1,
                    f(c.r),
                    f(c.g),
                    f(c.b),
                    comment(*share, "//", "")
                );
            }
            s.push_str("}\n");
        }
        Code::Tailwind3 => {
            s.push_str(
                "// tailwind.config.js\nmodule.exports = {\n  theme: {\n    extend: {\n      colors: {\n        palette: {\n",
            );
            for (i, (c, share)) in colours.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "          {}: '{}',{}",
                    i + 1,
                    c.hex(),
                    comment(*share, "//", "")
                );
            }
            s.push_str("        },\n      },\n    },\n  },\n};\n");
        }
    }
    s
}

/// A share as a whole percent, or `<1%` for a sliver.
pub fn percent(share: f32) -> String {
    let p = (share * 100.0).round();
    if p < 1.0 {
        "<1%".to_string()
    } else {
        format!("{p:.0}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` picture with each pixel's colour from `f`.
    fn pic(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Image {
        crate::ascii::pic(w, h, f)
    }

    /// Steps through a tiny PRNG so the noise is the same on every run.
    fn noise(seed: &mut u32) -> i16 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed % 9) as i16 - 4
    }

    #[test]
    fn two_flat_colours_give_two_with_their_shares() {
        // A quarter red, three quarters blue.
        let img = pic(40, 40, |x, _| {
            if x < 10 {
                [220, 30, 30, 255]
            } else {
                [20, 40, 200, 255]
            }
        });
        let p = palette(&img, 6).unwrap();
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!(p[0].colour, Colour::rgb(20, 40, 200));
        assert!((p[0].share - 0.75).abs() < 1e-6);
        assert_eq!(p[1].colour, Colour::rgb(220, 30, 30));
        assert!((p[1].share - 0.25).abs() < 1e-6);
    }

    #[test]
    fn noise_around_a_colour_is_one_colour() {
        let mut seed = 7u32;
        let rgba: Vec<u8> = (0..64 * 64)
            .flat_map(|i| {
                let base: [i16; 3] = if i % 64 < 32 {
                    [240, 200, 40]
                } else {
                    [30, 120, 60]
                };
                let mut px = [0u8; 4];
                for c in 0..3 {
                    px[c] = (base[c] + noise(&mut seed)).clamp(0, 255) as u8;
                }
                px[3] = 255;
                px
            })
            .collect();
        let img = Image {
            width: 64,
            height: 64,
            rgba,
        };
        let p = palette(&img, 6).unwrap();
        assert_eq!(p.len(), 2, "{p:?}");
        let near = |c: Colour, t: [u8; 3]| {
            (i16::from(c.r) - i16::from(t[0])).abs() <= 4
                && (i16::from(c.g) - i16::from(t[1])).abs() <= 4
                && (i16::from(c.b) - i16::from(t[2])).abs() <= 4
        };
        assert!(p.iter().any(|s| near(s.colour, [240, 200, 40])), "{p:?}");
        assert!(p.iter().any(|s| near(s.colour, [30, 120, 60])), "{p:?}");
    }

    #[test]
    fn six_distinct_colours_are_found_and_ordered_by_share() {
        let colours = [
            [255, 255, 255],
            [0, 0, 0],
            [230, 40, 40],
            [40, 180, 70],
            [40, 80, 220],
            [250, 210, 30],
        ];
        // Columns of widths 30, 25, 20, 12, 8, 5 out of 100.
        let widths = [30, 25, 20, 12, 8, 5];
        let img = pic(100, 10, |x, _| {
            let mut edge = 0;
            for (i, w) in widths.iter().enumerate() {
                edge += w;
                if x < edge {
                    let c = colours[i];
                    return [c[0], c[1], c[2], 255];
                }
            }
            unreachable!()
        });
        let p = palette(&img, 6).unwrap();
        let got: Vec<[u8; 3]> = p
            .iter()
            .map(|s| [s.colour.r, s.colour.g, s.colour.b])
            .collect();
        assert_eq!(got, colours);
        let shares: Vec<u32> = p.iter().map(|s| (s.share * 100.0).round() as u32).collect();
        assert_eq!(shares, widths);
    }

    #[test]
    fn a_colour_is_one_from_the_picture_not_a_blend() {
        // An orange block with soft edges into white: the orange shown is
        // the block's own, not pulled toward the white by its edges.
        let img = pic(100, 10, |x, _| match x {
            0..=59 => [255, 255, 255, 255],
            60 => [250, 210, 150, 255],
            61 => [248, 180, 80, 255],
            _ => [245, 158, 11, 255],
        });
        let p = palette(&img, 6).unwrap();
        assert!(
            p.iter().any(|s| s.colour == Colour::rgb(245, 158, 11)),
            "{p:?}"
        );
        assert_eq!(p[0].colour, Colour::rgb(255, 255, 255));
    }

    #[test]
    fn asking_for_fewer_colours_gives_fewer() {
        let img = pic(30, 1, |x, _| match x / 10 {
            0 => [255, 0, 0, 255],
            1 => [0, 255, 0, 255],
            _ => [0, 0, 255, 255],
        });
        assert_eq!(palette(&img, 2).unwrap().len(), 2);
        assert_eq!(palette(&img, 1).unwrap().len(), 1);
    }

    #[test]
    fn clear_pixels_are_left_out() {
        let img = pic(10, 10, |x, _| {
            if x < 5 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 0]
            }
        });
        let p = palette(&img, 6).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].colour, Colour::rgb(255, 0, 0));
        assert!((p[0].share - 1.0).abs() < 1e-6);
        let clear = pic(4, 4, |_, _| [0, 0, 0, 0]);
        let e = palette(&clear, 6).unwrap_err();
        assert!(
            e.message.contains("transparent everywhere"),
            "{}",
            e.message
        );
    }

    #[test]
    fn the_same_picture_gives_the_same_palette() {
        let img = pic(64, 64, |x, y| {
            [(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]
        });
        let a = palette(&img, 6).unwrap();
        assert_eq!(a.len(), 6);
        assert_eq!(a, palette(&img, 6).unwrap());
        let sum: f32 = a.iter().map(|s| s.share).sum();
        assert!((sum - 1.0).abs() < 1e-4, "{sum}");
    }

    #[test]
    fn oklab_round_trips_every_grey_and_the_primaries() {
        for v in 0..=255u8 {
            let lab = oklab([f32::from(v); 3]);
            assert_eq!(from_oklab(lab), Colour::rgb(v, v, v), "{v}");
        }
        for c in [[255, 0, 0], [0, 255, 0], [0, 0, 255], [12, 200, 99]] {
            let lab = oklab(c.map(f32::from));
            assert_eq!(from_oklab(lab), Colour::rgb(c[0], c[1], c[2]));
        }
    }

    #[test]
    fn the_colour_at_a_pixel_and_outside() {
        let img = pic(3, 2, |x, y| [x as u8, y as u8, 7, 128]);
        assert_eq!(
            at(&img, 2, 1).unwrap(),
            Colour {
                r: 2,
                g: 1,
                b: 7,
                a: 128
            }
        );
        let e = at(&img, 3, 0).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("3x2"), "{}", e.message);
    }

    #[test]
    fn colours_are_written_as_hex_rgb_and_hsl() {
        let c = Colour::rgb(43, 74, 111);
        assert_eq!(c.hex(), "#2b4a6f");
        assert_eq!(c.rgb_css(), "rgb(43, 74, 111)");
        assert_eq!(c.hsl_css(), "hsl(213, 44%, 30%)");
        assert_eq!(Colour::rgb(255, 0, 0).hsl(), [0, 100, 50]);
        assert_eq!(Colour::rgb(0, 255, 0).hsl(), [120, 100, 50]);
        assert_eq!(Colour::rgb(0, 0, 255).hsl(), [240, 100, 50]);
        assert_eq!(Colour::rgb(255, 0, 128).hsl(), [330, 100, 50]);
        assert_eq!(Colour::rgb(128, 128, 128).hsl(), [0, 0, 50]);
        let half = Colour { a: 128, ..c };
        assert_eq!(half.hex(), "#2b4a6f80");
        assert_eq!(half.rgb_css(), "rgba(43, 74, 111, 0.502)");
        assert_eq!(Colour { a: 0, ..c }.hsl_css(), "hsla(213, 44%, 30%, 0)");
    }

    #[test]
    fn code_in_each_language() {
        let colours = [
            (Colour::rgb(43, 74, 111), Some(0.62)),
            (Colour::rgb(255, 255, 255), Some(0.004)),
        ];
        assert_eq!(
            code(&colours, Code::Css),
            ":root {\n  --palette-1: #2b4a6f; /* 62% */\n  --palette-2: #ffffff; /* <1% */\n}\n"
        );
        assert_eq!(
            code(&colours, Code::Tailwind),
            "@theme {\n  --color-palette-1: #2b4a6f; /* 62% */\n  --color-palette-2: #ffffff; /* <1% */\n}\n"
        );
        let swift = code(&colours, Code::Swiftui);
        assert!(
            swift.starts_with("import SwiftUI\n\nextension Color {\n"),
            "{swift}"
        );
        assert!(
            swift.contains(
                "    static let palette1 = Color(red: 0.169, green: 0.290, blue: 0.435) // 62%\n"
            ),
            "{swift}"
        );
        let js = code(&colours, Code::Tailwind3);
        assert!(
            js.contains("        palette: {\n          1: '#2b4a6f', // 62%\n"),
            "{js}"
        );
        assert!(js.ends_with("};\n"), "{js}");
        // No share, no comment; an opacity where there is one.
        let one = [(
            Colour {
                a: 128,
                ..Colour::rgb(1, 2, 3)
            },
            None,
        )];
        assert_eq!(
            code(&one, Code::Css),
            ":root {\n  --palette-1: #01020380;\n}\n"
        );
        assert!(code(&one, Code::Swiftui).contains("opacity: 0.502)\n"));
    }

    #[test]
    fn code_names_parse() {
        assert_eq!(Code::parse("CSS"), Ok(Code::Css));
        assert_eq!(Code::parse("swift"), Ok(Code::Swiftui));
        assert_eq!(Code::parse("tailwind"), Ok(Code::Tailwind));
        assert_eq!(Code::parse("tailwind3"), Ok(Code::Tailwind3));
        assert!(Code::parse("scss").unwrap_err().contains("css, swiftui"));
    }
}
