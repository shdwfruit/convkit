//! The picture sampled onto a grid, and the tone work on it.

use super::Image;

/// A picture averaged down (or repeated up) to `w` x `h` samples, row by
/// row: sRGB from 0 to 1, already laid over the background, with how
/// opaque each sample was.
#[derive(Debug, Clone)]
pub struct Grid {
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<[f32; 3]>,
    pub alpha: Vec<f32>,
}

/// For each of `out` samples along a side of `src` pixels, the pixels it
/// covers and by how much.
fn spans(src: u32, out: u32) -> Vec<Vec<(usize, f32)>> {
    let step = src as f64 / out as f64;
    (0..out)
        .map(|i| {
            let (a, b) = (i as f64 * step, (i + 1) as f64 * step);
            let first = a.floor() as u32;
            let last = (b.ceil() as u32).clamp(first + 1, src);
            (first..last)
                .filter_map(|p| {
                    let cover = (b.min(p as f64 + 1.0) - a.max(p as f64)) as f32;
                    (cover > 0.0).then_some((p as usize, cover))
                })
                .collect()
        })
        .collect()
}

/// Samples `img` onto a `w` x `h` grid: each sample is the area average of
/// the pixels under it, transparency weighted, laid over `bg`.
pub fn sample(img: &Image, w: u32, h: u32, bg: [u8; 3]) -> Grid {
    let (xs, ys) = (spans(img.width, w), spans(img.height, h));
    let bg = bg.map(|c| c as f32 / 255.0);
    let stride = img.width as usize * 4;
    let mut rgb = Vec::with_capacity((w * h) as usize);
    let mut alpha = Vec::with_capacity((w * h) as usize);
    for ys in &ys {
        for xs in &xs {
            let (mut sum, mut a_sum, mut weight) = ([0f32; 3], 0f32, 0f32);
            for &(y, wy) in ys {
                for &(x, wx) in xs {
                    let px = &img.rgba[y * stride + x * 4..][..4];
                    let (k, a) = (wy * wx, px[3] as f32 / 255.0);
                    for c in 0..3 {
                        sum[c] += k * a * px[c] as f32 / 255.0;
                    }
                    a_sum += k * a;
                    weight += k;
                }
            }
            let a = if weight > 0.0 { a_sum / weight } else { 0.0 };
            let premult = sum.map(|c| if weight > 0.0 { c / weight } else { 0.0 });
            rgb.push([0, 1, 2].map(|c| premult[c] + (1.0 - a) * bg[c]));
            alpha.push(a);
        }
    }
    Grid { w, h, rgb, alpha }
}

/// Brightness, Rec. 709.
pub fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Stretches `v` so its opaque samples run from 0 (the 2nd percentile) to
/// 1 (the 98th): the automatic contrast that keeps a photo from turning
/// to grey mush. A flat picture, or one with almost nothing opaque, is
/// left as it is.
pub fn levels(v: &mut [f32], alpha: &[f32]) {
    let mut opaque: Vec<f32> = v
        .iter()
        .zip(alpha)
        .filter(|(_, &a)| a >= 0.5)
        .map(|(&x, _)| x)
        .collect();
    if opaque.len() < 2 {
        return;
    }
    opaque.sort_by(f32::total_cmp);
    let at = |q: f32| opaque[((opaque.len() - 1) as f32 * q).round() as usize];
    let (lo, hi) = (at(0.02), at(0.98));
    if hi - lo < 1e-3 {
        return;
    }
    for x in v.iter_mut() {
        *x = ((*x - lo) / (hi - lo)).clamp(0.0, 1.0);
    }
}

/// `--contrast` and `--brightness`, each -100 to 100, 0 leaving `v` as it
/// is. Contrast 50 doubles the distance from mid-grey, 100 quadruples it.
pub fn adjust(v: f32, contrast: i32, brightness: i32) -> f32 {
    ((v - 0.5) * 2f32.powf(contrast as f32 / 50.0) + 0.5 + brightness as f32 / 200.0)
        .clamp(0.0, 1.0)
}

/// Whether the picture's shapes differ mostly in colour rather than
/// brightness (a red parrot on green leaves), so styles that only see
/// brightness may lose them. Over neighbouring opaque samples that differ
/// noticeably, compares how much colour and how much brightness change.
/// Only noticeable pairs count, so the answer doesn't depend on how fine
/// the grid is, and a flat or gently shaded picture never qualifies.
pub fn colour_only(grid: &Grid) -> bool {
    const NOTICEABLE: f32 = 0.05;
    let (w, h) = (grid.w as usize, grid.h as usize);
    let (mut sum_c, mut sum_l, mut pairs) = (0f32, 0f32, 0usize);
    let mut pair = |i: usize, j: usize| {
        if grid.alpha[i] < 0.5 || grid.alpha[j] < 0.5 {
            return;
        }
        let (a, b) = (grid.rgb[i], grid.rgb[j]);
        let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
            / 3f32.sqrt();
        if d <= NOTICEABLE {
            return;
        }
        sum_c += d;
        sum_l += (luminance(a) - luminance(b)).abs();
        pairs += 1;
    };
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if x + 1 < w {
                pair(i, i + 1);
            }
            if y + 1 < h {
                pair(i, i + w);
            }
        }
    }
    pairs > 0 && sum_c > 2.5 * sum_l
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascii::pic;

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn sample_averages_area_and_composites_onto_the_background() {
        let img = pic(2, 1, |x, _| if x == 0 { [255; 4] } else { [0, 0, 0, 255] });
        let g = sample(&img, 1, 1, [0, 0, 0]);
        assert!(g.rgb[0].iter().all(|&c| near(c, 0.5)), "{:?}", g.rgb[0]);
        assert!(near(g.alpha[0], 1.0));

        let clear = pic(1, 1, |_, _| [255, 255, 255, 0]);
        let g = sample(&clear, 1, 1, [12, 12, 14]);
        assert!(near(g.rgb[0][0], 12.0 / 255.0), "{:?}", g.rgb[0]);
        assert!(near(g.rgb[0][2], 14.0 / 255.0));
        assert_eq!(g.alpha[0], 0.0);

        let tiny = pic(2, 2, |x, y| [(x * 255) as u8, (y * 255) as u8, 0, 255]);
        let g = sample(&tiny, 100, 50, [0, 0, 0]);
        assert_eq!(
            (g.w, g.h, g.rgb.len(), g.alpha.len()),
            (100, 50, 5000, 5000)
        );
        assert!(near(g.rgb[0][0], 0.0) && near(g.rgb[99][0], 1.0));
    }

    #[test]
    fn levels_stretches_opaque_samples_only() {
        let mut v = [0.4, 0.5, 0.6, 0.0];
        levels(&mut v, &[1.0, 1.0, 1.0, 0.0]);
        assert!(
            near(v[0], 0.0) && near(v[1], 0.5) && near(v[2], 1.0),
            "{v:?}"
        );
        assert_eq!(v[3], 0.0);

        let mut flat = [0.3; 4];
        levels(&mut flat, &[1.0; 4]);
        assert_eq!(flat, [0.3; 4]);
    }

    #[test]
    fn adjust_is_identity_at_zero() {
        assert!(near(adjust(0.3, 0, 0), 0.3));
    }

    #[test]
    fn adjust_contrast_spreads() {
        assert_eq!(adjust(0.75, 50, 0), 1.0);
        assert_eq!(adjust(0.25, 50, 0), 0.0);
        assert!(adjust(0.5, 0, 50) > 0.5 && adjust(0.5, 0, -50) < 0.5);
    }

    #[test]
    fn colour_only_spots_equal_brightness_colours() {
        let halves = |a: [u8; 4], b: [u8; 4]| {
            let img = pic(40, 20, |x, _| if x < 20 { a } else { b });
            sample(&img, 20, 10, [0, 0, 0])
        };
        assert!(colour_only(&halves([255, 0, 0, 255], [0, 76, 0, 255])));
        assert!(!colour_only(&halves([0, 0, 0, 255], [255, 255, 255, 255])));
        let ramp = sample(
            &pic(40, 20, |x, _| {
                let v = (x * 6) as u8;
                [v, v, v, 255]
            }),
            20,
            10,
            [0, 0, 0],
        );
        assert!(!colour_only(&ramp));
        assert!(!colour_only(&halves([255, 0, 0, 255], [255, 0, 0, 255])));
    }
}
