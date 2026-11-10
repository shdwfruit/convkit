//! A screenshot set on a background with padding, rounded corners and a
//! soft shadow (`conv beautify`), drawn by conv itself, with no backend.
//!
//! The screenshot is copied pixel for pixel: it is never resampled, so a
//! 2x (Retina) screenshot stays exactly as sharp, and its colour profile
//! and pixel density go into the PNG with it, so it opens at the size it
//! was taken at. Lengths given as flags are in points, the pixels of a 1x
//! screen, and are multiplied by the screenshot's scale (2 for a 144 dpi
//! PNG, which is what macOS writes on a Retina screen). Left to conv, they
//! grow with the screenshot instead, so a small crop and a whole screen
//! both come out looking the same.
//!
//! A picture whose corners are already clear (a macOS window captured with
//! its shadow, a logo) has its own shape: by default it keeps it, with no
//! corners cut and no second shadow.

use resvg::tiny_skia::{
    FillRule, Mask, PathBuilder, Pixmap, PixmapPaint, PremultipliedColorU8, Transform,
};

use crate::picture::Picture;
use crate::{palette, ConvError, ErrorCode, Result};

/// The gradients `--bg` names, each from the top-left corner to the
/// bottom-right one.
pub const PRESETS: &[(&str, &[[u8; 3]])] = &[
    (
        "aurora",
        &[[0x7F, 0x7F, 0xD5], [0x86, 0xA8, 0xE7], [0x91, 0xEA, 0xE4]],
    ),
    ("sunset", &[[0xFF, 0x7E, 0x5F], [0xFE, 0xB4, 0x7B]]),
    ("candy", &[[0xFB, 0xC2, 0xEB], [0xA6, 0xC1, 0xEE]]),
    ("ocean", &[[0x2E, 0x31, 0x92], [0x1B, 0xFF, 0xFF]]),
    ("mint", &[[0xD4, 0xFC, 0x79], [0x96, 0xE6, 0xA1]]),
    (
        "night",
        &[[0x0F, 0x20, 0x27], [0x20, 0x3A, 0x43], [0x2C, 0x53, 0x64]],
    ),
];

/// The background when `--bg` isn't given.
pub const DEFAULT_PRESET: &str = "aurora";

/// Padding left to conv: this fraction of the screenshot's size (the
/// square root of its area, so a long strip isn't swamped).
const PADDING: f32 = 0.09;
/// Corner radius left to conv, the same way.
const RADIUS: f32 = 0.015;
/// The shadow's blur (its Gaussian sigma) and how far down it falls.
const BLUR: f32 = 0.024;
const DROP: f32 = 0.015;
/// How dark the shadow is where it is darkest.
const SHADOW_OPACITY: f32 = 0.45;

/// The largest picture conv draws, in pixels on a side and in all. A
/// bigger one takes gigabytes of memory to draw.
const MAX_SIDE: u32 = 20_000;
const MAX_PIXELS: u64 = 150_000_000;

/// What goes behind the screenshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Background {
    /// From the top-left corner to the bottom-right one, through each
    /// colour in turn.
    Gradient(Vec<[u8; 3]>),
    Solid([u8; 3]),
    Transparent,
}

impl Background {
    /// Reads `--bg`: a preset's name, `transparent`, a colour (`#1e1e2e`,
    /// `#fff`), or colours joined by commas for a gradient of them.
    pub fn parse(s: &str) -> std::result::Result<Background, String> {
        let t = s.trim();
        let lower = t.to_ascii_lowercase();
        if let Some((_, stops)) = PRESETS.iter().find(|(name, _)| *name == lower) {
            return Ok(Background::Gradient(stops.to_vec()));
        }
        if matches!(lower.as_str(), "transparent" | "none" | "clear") {
            return Ok(Background::Transparent);
        }
        let colours: std::result::Result<Vec<[u8; 3]>, ()> =
            t.split(',').map(|c| hex_colour(c.trim())).collect();
        match colours {
            Ok(c) if c.len() == 1 => Ok(Background::Solid(c[0])),
            Ok(c) => Ok(Background::Gradient(c)),
            Err(()) => {
                let names: Vec<&str> = PRESETS.iter().map(|p| p.0).collect();
                Err(format!(
                    "expected {}, transparent, a colour like #1e1e2e, or colours joined by commas; got {s:?}",
                    names.join(", ")
                ))
            }
        }
    }

    /// How the background is described back: its preset's name, or its
    /// colours.
    pub fn describe(&self) -> String {
        match self {
            Background::Transparent => "transparent".to_string(),
            Background::Solid(c) => hex(*c),
            Background::Gradient(stops) => PRESETS
                .iter()
                .find(|(_, s)| *s == stops.as_slice())
                .map_or_else(
                    || stops.iter().map(|c| hex(*c)).collect::<Vec<_>>().join(","),
                    |(name, _)| name.to_string(),
                ),
        }
    }
}

fn hex(c: [u8; 3]) -> String {
    palette::Colour::rgb(c[0], c[1], c[2]).hex()
}

/// `#rrggbb`, `rrggbb`, `#rgb` or `rgb`.
fn hex_colour(s: &str) -> std::result::Result<[u8; 3], ()> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(());
    }
    let digit = |i: usize, len: usize| u8::from_str_radix(&h[i..i + len], 16).map_err(|_| ());
    match h.len() {
        6 => Ok([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?]),
        3 => {
            let d = |i| digit(i, 1).map(|v| v * 17);
            Ok([d(0)?, d(1)?, d(2)?])
        }
        _ => Err(()),
    }
}

/// What was asked for. `None` leaves it to conv.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub background: Background,
    /// Points on each side.
    pub padding: Option<u32>,
    /// The corners' radius in points; 0 keeps them square.
    pub radius: Option<u32>,
    pub shadow: Option<bool>,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            background: Background::parse(DEFAULT_PRESET).expect("a preset"),
            padding: None,
            radius: None,
            shadow: None,
        }
    }
}

/// What will be drawn, in pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    /// The screenshot's scale: 2 for a 2x (Retina) screenshot.
    pub scale: u32,
    pub padding: u32,
    pub radius: u32,
    pub shadow: bool,
    /// The picture's corners were already clear, so it kept its shape.
    pub own_shape: bool,
    pub background: Background,
    blur: f32,
    drop: i32,
}

/// The scale a screenshot was taken at, from its pixel density: 144 dpi
/// is 2x on macOS (72 a point), and 192 is 2x on Windows (96 a point).
/// Anything else, or none, is 1x.
pub fn scale_of(dpi: Option<f32>) -> u32 {
    let near = |d: f32, v: f32| (d - v).abs() < 2.0;
    match dpi {
        Some(d) if near(d, 144.0) || near(d, 192.0) => 2,
        Some(d) if near(d, 216.0) || near(d, 288.0) => 3,
        _ => 1,
    }
}

/// Whether all four corners of the picture are clear.
fn has_own_shape(pic: &Picture) -> bool {
    let img = &pic.image;
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 {
        return false;
    }
    [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]
        .iter()
        .all(|&(x, y)| img.rgba[(y * w + x) * 4 + 3] < 8)
}

/// Works out the picture `style` makes of `pic`, without drawing it.
pub fn plan(pic: &Picture, style: &Style) -> Result<Plan> {
    let (w, h) = (pic.image.width, pic.image.height);
    if w == 0 || h == 0 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "the picture is empty",
        ));
    }
    let scale = scale_of(pic.dpi);
    let own_shape = has_own_shape(pic);
    let base = ((w as f32) * (h as f32)).sqrt();
    let points = |v: u32| v.saturating_mul(scale);
    let padding = style
        .padding
        .map_or_else(|| (base * PADDING).round() as u32, points);
    let radius = match style.radius {
        Some(r) => points(r),
        None if own_shape => 0,
        None => ((base * RADIUS).round() as u32).max(2 * scale),
    }
    .min(w.min(h) / 2);
    let shadow = style.shadow.unwrap_or(!own_shape);
    let width = u64::from(w) + 2 * u64::from(padding);
    let height = u64::from(h) + 2 * u64::from(padding);
    if width > u64::from(MAX_SIDE) || height > u64::from(MAX_SIDE) || width * height > MAX_PIXELS {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "that makes a {width}x{height} picture, larger than conv draws ({MAX_SIDE} pixels a side, {} megapixels); give a smaller --padding",
                MAX_PIXELS / 1_000_000
            ),
        ));
    }
    Ok(Plan {
        width: width as u32,
        height: height as u32,
        scale,
        padding,
        radius,
        shadow,
        own_shape,
        background: style.background.clone(),
        blur: base * BLUR,
        drop: (base * DROP).round() as i32,
    })
}

/// Draws `plan` for `pic`, as a PNG.
pub fn render(pic: &Picture, plan: &Plan) -> Result<Vec<u8>> {
    let failed = |what: &str| ConvError::new(ErrorCode::ConversionFailed, what.to_string());
    let mut canvas = Pixmap::new(plan.width, plan.height)
        .ok_or_else(|| failed("cannot draw a picture that large"))?;
    fill_background(&mut canvas, &plan.background);

    let shot = shaped(pic, plan.radius).ok_or_else(|| failed("cannot draw the picture"))?;
    if plan.shadow {
        let pad = plan.padding as i32;
        cast_shadow(&mut canvas, &shot, pad, pad + plan.drop, plan.blur);
    }
    canvas.draw_pixmap(
        plan.padding as i32,
        plan.padding as i32,
        shot.as_ref(),
        &PixmapPaint {
            quality: resvg::tiny_skia::FilterQuality::Nearest,
            ..PixmapPaint::default()
        },
        Transform::identity(),
        None,
    );
    encode(&canvas, pic)
}

/// The screenshot as a premultiplied pixmap, its corners cut to `radius`.
fn shaped(pic: &Picture, radius: u32) -> Option<Pixmap> {
    let img = &pic.image;
    let mut pixmap = Pixmap::new(img.width, img.height)?;
    for (dst, p) in pixmap
        .pixels_mut()
        .iter_mut()
        .zip(img.rgba.as_chunks::<4>().0)
    {
        let a = u16::from(p[3]);
        let pre = |v: u8| ((u16::from(v) * a + 127) / 255) as u8;
        *dst = PremultipliedColorU8::from_rgba(pre(p[0]), pre(p[1]), pre(p[2]), p[3])?;
    }
    if radius > 0 {
        let mut mask = Mask::new(img.width, img.height)?;
        let path = rounded_rect(img.width as f32, img.height as f32, radius as f32)?;
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        pixmap.apply_mask(&mask);
    }
    Some(pixmap)
}

/// A `w`x`h` rectangle at the origin with corners of radius `r`.
fn rounded_rect(w: f32, h: f32, r: f32) -> Option<resvg::tiny_skia::Path> {
    // The control point distance that makes a cubic follow a quarter circle.
    let k = r * 0.552_284_8;
    let mut p = PathBuilder::new();
    p.move_to(r, 0.0);
    p.line_to(w - r, 0.0);
    p.cubic_to(w - r + k, 0.0, w, r - k, w, r);
    p.line_to(w, h - r);
    p.cubic_to(w, h - r + k, w - r + k, h, w - r, h);
    p.line_to(r, h);
    p.cubic_to(r - k, h, 0.0, h - r + k, 0.0, h - r);
    p.line_to(0.0, r);
    p.cubic_to(0.0, r - k, r - k, 0.0, r, 0.0);
    p.close();
    p.finish()
}

/// Fills the canvas. A gradient is mixed in OKLab, so it passes through
/// clean colours rather than grey, and dithered, so a wide one shows no
/// bands.
fn fill_background(canvas: &mut Pixmap, bg: &Background) {
    let stops: Vec<[f32; 3]> = match bg {
        Background::Transparent => return,
        Background::Solid(c) => {
            let px = PremultipliedColorU8::from_rgba(c[0], c[1], c[2], 255).expect("opaque");
            canvas.pixels_mut().fill(px);
            return;
        }
        Background::Gradient(stops) => stops
            .iter()
            .map(|c| palette::oklab(c.map(f32::from)))
            .collect(),
    };
    // The colour along the diagonal, worked out once for each of these
    // steps rather than for every pixel.
    const STEPS: usize = 4096;
    let ramp: Vec<[f32; 3]> = (0..STEPS)
        .map(|i| {
            let t = i as f32 / (STEPS - 1) as f32;
            let span = t * (stops.len() - 1) as f32;
            let at = (span.floor() as usize).min(stops.len().saturating_sub(2));
            let f = span - at as f32;
            let (a, b) = (stops[at], stops[(at + 1).min(stops.len() - 1)]);
            let lab = [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * f);
            palette::oklab_to_srgb_f32(lab)
        })
        .collect();
    // A 4x4 Bayer matrix, as offsets of -0.5 to 0.5 of a level.
    const BAYER: [[f32; 4]; 4] = [
        [0.0, 8.0, 2.0, 10.0],
        [12.0, 4.0, 14.0, 6.0],
        [3.0, 11.0, 1.0, 9.0],
        [15.0, 7.0, 13.0, 5.0],
    ];
    let (w, h) = (canvas.width() as usize, canvas.height() as usize);
    let pixels = canvas.pixels_mut();
    for y in 0..h {
        for x in 0..w {
            // Top-left 0, bottom-right 1, the other two corners halfway.
            let t = (x as f32 / (w.max(2) - 1) as f32 + y as f32 / (h.max(2) - 1) as f32) / 2.0;
            let c = ramp[((t * (STEPS - 1) as f32).round() as usize).min(STEPS - 1)];
            let d = BAYER[y % 4][x % 4] / 16.0 - 0.469;
            let v = |v: f32| (v * 255.0 + d).round().clamp(0.0, 255.0) as u8;
            pixels[y * w + x] =
                PremultipliedColorU8::from_rgba(v(c[0]), v(c[1]), v(c[2]), 255).expect("opaque");
        }
    }
}

/// Darkens the canvas under `shot` placed at `(x, y)`: its own shape,
/// blurred by `sigma`.
fn cast_shadow(canvas: &mut Pixmap, shot: &Pixmap, x: i32, y: i32, sigma: f32) {
    let margin = (sigma * 3.0).ceil().max(0.0) as usize;
    let (sw, sh) = (shot.width() as usize, shot.height() as usize);
    let (bw, bh) = (sw + 2 * margin, sh + 2 * margin);
    let mut alpha = vec![0f32; bw * bh];
    for (i, p) in shot.pixels().iter().enumerate() {
        let (px, py) = (i % sw, i / sw);
        alpha[(py + margin) * bw + px + margin] = f32::from(p.alpha()) / 255.0;
    }
    if sigma >= 0.5 {
        blur(&mut alpha, bw, bh, sigma);
    }
    let (cw, ch) = (canvas.width() as i32, canvas.height() as i32);
    let (ox, oy) = (x - margin as i32, y - margin as i32);
    let pixels = canvas.pixels_mut();
    for by in 0..bh as i32 {
        let cy = oy + by;
        if cy < 0 || cy >= ch {
            continue;
        }
        for bx in 0..bw as i32 {
            let cx = ox + bx;
            if cx < 0 || cx >= cw {
                continue;
            }
            let a = alpha[(by as usize) * bw + bx as usize] * SHADOW_OPACITY;
            if a <= 0.0 {
                continue;
            }
            let dst = &mut pixels[(cy * cw + cx) as usize];
            let keep = 1.0 - a;
            let scale = |v: u8| (f32::from(v) * keep).round() as u8;
            let new_alpha = (a * 255.0 + f32::from(dst.alpha()) * keep).round() as u8;
            // Black over the pixel: the colour fades, the cover grows.
            *dst = PremultipliedColorU8::from_rgba(
                scale(dst.red()),
                scale(dst.green()),
                scale(dst.blue()),
                new_alpha,
            )
            .unwrap_or(*dst);
        }
    }
}

/// An approximate Gaussian blur: three box blurs across, then three down.
fn blur(buf: &mut [f32], w: usize, h: usize, sigma: f32) {
    // The box width whose three passes have this sigma.
    let box_w = ((12.0 * sigma * sigma / 3.0 + 1.0).sqrt()).round() as usize;
    let r = box_w.max(1) / 2;
    let mut tmp = vec![0f32; buf.len()];
    for _ in 0..3 {
        box_pass(buf, &mut tmp, w, h, r, true);
        box_pass(&tmp, buf, w, h, r, false);
    }
}

/// One box blur of radius `r` along rows (`across`) or columns, from
/// `src` into `dst`, treating everything past the edge as clear.
fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, across: bool) {
    let (lines, len) = if across { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if across { line * w + i } else { i * w + line };
    let norm = 1.0 / (2 * r + 1) as f32;
    for line in 0..lines {
        let mut sum: f32 = (0..=r.min(len - 1)).map(|i| src[at(line, i)]).sum();
        for i in 0..len {
            dst[at(line, i)] = sum * norm;
            if i + r + 1 < len {
                sum += src[at(line, i + r + 1)];
            }
            if i >= r {
                sum -= src[at(line, i - r)];
            }
        }
    }
}

/// The canvas as a PNG with the screenshot's density and colour profile:
/// RGB when nothing is see-through, RGBA otherwise.
fn encode(canvas: &Pixmap, pic: &Picture) -> Result<Vec<u8>> {
    let opaque = canvas.pixels().iter().all(|p| p.alpha() == 255);
    let (colour, data): (png::ColorType, Vec<u8>) = if opaque {
        let rgb = canvas
            .pixels()
            .iter()
            .flat_map(|p| [p.red(), p.green(), p.blue()])
            .collect();
        (png::ColorType::Rgb, rgb)
    } else {
        let rgba = canvas
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        (png::ColorType::Rgba, rgba)
    };
    let mut info = png::Info::with_size(canvas.width(), canvas.height());
    info.color_type = colour;
    info.bit_depth = png::BitDepth::Eight;
    info.pixel_dims = pic.dpi.map(|dpi| {
        let ppm = (dpi / 0.0254).round() as u32;
        png::PixelDimensions {
            xppu: ppm,
            yppu: ppm,
            unit: png::Unit::Meter,
        }
    });
    if let Some(icc) = &pic.icc {
        info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
    } else if pic.srgb {
        info.srgb = Some(png::SrgbRenderingIntent::Perceptual);
    }
    let failed = |e: png::EncodingError| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not write the PNG: {e}"),
        )
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::with_info(&mut out, info).map_err(failed)?;
    encoder.set_compression(png::Compression::Balanced);
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(&data).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascii::Image;

    fn shot(w: u32, h: u32, dpi: Option<f32>) -> Picture {
        Picture {
            image: Image {
                width: w,
                height: h,
                rgba: [250, 250, 250, 255].repeat((w * h) as usize),
            },
            dpi,
            icc: None,
            srgb: false,
        }
    }

    fn decoded(png: &[u8]) -> Picture {
        crate::picture::decode(png, crate::Format::Png).unwrap()
    }

    fn px(p: &Picture, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * p.image.width + x) * 4) as usize;
        p.image.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn backgrounds_parse() {
        assert_eq!(
            Background::parse("Sunset").unwrap(),
            Background::Gradient(vec![[0xFF, 0x7E, 0x5F], [0xFE, 0xB4, 0x7B]])
        );
        assert_eq!(
            Background::parse("transparent").unwrap(),
            Background::Transparent
        );
        assert_eq!(
            Background::parse("#1e1e2e").unwrap(),
            Background::Solid([0x1e, 0x1e, 0x2e])
        );
        assert_eq!(
            Background::parse("fff").unwrap(),
            Background::Solid([255; 3])
        );
        assert_eq!(
            Background::parse("#000, #f00").unwrap(),
            Background::Gradient(vec![[0, 0, 0], [255, 0, 0]])
        );
        let e = Background::parse("purple").unwrap_err();
        assert!(e.contains("aurora, sunset"), "{e}");
        assert!(Background::parse("#12345").is_err());
        assert!(Background::parse("#12345g").is_err());
    }

    #[test]
    fn a_background_describes_itself() {
        assert_eq!(Background::parse("ocean").unwrap().describe(), "ocean");
        assert_eq!(Background::parse("#ABC").unwrap().describe(), "#aabbcc");
        assert_eq!(
            Background::parse("#000,#fff").unwrap().describe(),
            "#000000,#ffffff"
        );
    }

    #[test]
    fn the_scale_comes_from_the_density() {
        assert_eq!(scale_of(None), 1);
        assert_eq!(scale_of(Some(72.0)), 1);
        assert_eq!(scale_of(Some(143.99)), 2);
        assert_eq!(scale_of(Some(192.0)), 2);
        assert_eq!(scale_of(Some(216.0)), 3);
        assert_eq!(scale_of(Some(300.0)), 1);
    }

    #[test]
    fn the_output_is_the_screenshot_plus_padding_at_its_own_scale() {
        let pic = shot(400, 300, Some(144.0));
        let style = Style {
            padding: Some(20),
            radius: Some(10),
            ..Style::default()
        };
        let p = plan(&pic, &style).unwrap();
        assert_eq!(p.scale, 2);
        // Points are doubled on a 2x screenshot.
        assert_eq!((p.padding, p.radius), (40, 20));
        assert_eq!((p.width, p.height), (480, 380));
        let out = decoded(&render(&pic, &p).unwrap());
        assert_eq!((out.image.width, out.image.height), (480, 380));
        assert!((out.dpi.unwrap() - 144.0).abs() < 0.1, "{:?}", out.dpi);
        // The screenshot is copied exactly: its middle is untouched.
        assert_eq!(px(&out, 240, 190), [250, 250, 250, 255]);
        assert_eq!(px(&out, 40 + 20, 40), [250, 250, 250, 255]);
    }

    #[test]
    fn corners_are_cut_and_the_background_shows_through() {
        let pic = shot(200, 100, None);
        let style = Style {
            background: Background::Solid([255, 0, 0]),
            padding: Some(10),
            radius: Some(20),
            shadow: Some(false),
        };
        let p = plan(&pic, &style).unwrap();
        let out = decoded(&render(&pic, &p).unwrap());
        // The screenshot's own corner pixel is gone: the red shows.
        assert_eq!(px(&out, 10, 10), [255, 0, 0, 255]);
        assert_eq!(px(&out, 209, 109), [255, 0, 0, 255]);
        // Inside the curve, and along an edge, the screenshot is there.
        assert_eq!(px(&out, 10 + 20, 10 + 20), [250, 250, 250, 255]);
        assert_eq!(px(&out, 10 + 100, 10), [250, 250, 250, 255]);
        // Square corners keep it.
        let square = Style {
            radius: Some(0),
            ..style
        };
        let p = plan(&pic, &square).unwrap();
        let out = decoded(&render(&pic, &p).unwrap());
        assert_eq!(px(&out, 10, 10), [250, 250, 250, 255]);
    }

    #[test]
    fn a_transparent_background_keeps_alpha_and_the_shadow_is_soft() {
        let pic = shot(100, 100, None);
        let style = Style {
            background: Background::Transparent,
            padding: Some(40),
            radius: Some(8),
            shadow: Some(true),
        };
        let p = plan(&pic, &style).unwrap();
        let png = render(&pic, &p).unwrap();
        // Colour type 6 is RGBA.
        assert_eq!(png[25], 6);
        let out = decoded(&png);
        assert_eq!(px(&out, 0, 0)[3], 0, "far from the picture, clear");
        // Just below the picture, the shadow: dark and partly see-through.
        let under = px(&out, 90, 40 + 100 + 2);
        assert!(under[3] > 10 && under[3] < 255, "{under:?}");
        assert!(under[0] < 10, "{under:?}");
        // A rounded corner is see-through too.
        assert!(px(&out, 40, 40)[3] < 255);
        assert_eq!(px(&out, 90, 90), [250, 250, 250, 255]);
    }

    #[test]
    fn an_opaque_result_is_written_as_rgb_and_keeps_the_profile() {
        let mut pic = shot(60, 40, Some(144.0));
        pic.icc = Some(b"fake profile bytes".to_vec());
        let p = plan(&pic, &Style::default()).unwrap();
        let png = render(&pic, &p).unwrap();
        // Colour type 2 is RGB.
        assert_eq!(png[25], 2);
        assert_eq!(
            decoded(&png).icc.as_deref(),
            Some(&b"fake profile bytes"[..])
        );
    }

    #[test]
    fn a_gradient_runs_corner_to_corner() {
        let pic = shot(100, 100, None);
        let style = Style {
            background: Background::parse("#000000,#ffffff").unwrap(),
            padding: Some(50),
            radius: Some(0),
            shadow: Some(false),
        };
        let p = plan(&pic, &style).unwrap();
        let out = decoded(&render(&pic, &p).unwrap());
        let top_left = px(&out, 0, 0);
        let bottom_right = px(&out, 199, 199);
        assert!(top_left[0] <= 1, "{top_left:?}");
        assert!(bottom_right[0] >= 254, "{bottom_right:?}");
        let middle = px(&out, 199, 0)[0];
        assert!(
            (80..=160).contains(&middle),
            "halfway is mid-grey: {middle}"
        );
    }

    #[test]
    fn defaults_grow_with_the_screenshot() {
        let small = plan(&shot(400, 300, None), &Style::default()).unwrap();
        let large = plan(&shot(2880, 1800, Some(144.0)), &Style::default()).unwrap();
        assert!(small.shadow && large.shadow);
        assert!(large.padding > 4 * small.padding, "{small:?} {large:?}");
        assert!(large.radius > small.radius);
        // Padding leaves room for the shadow to fade out.
        for p in [&small, &large] {
            assert!(p.padding as f32 > 3.0 * p.blur + p.drop as f32, "{p:?}");
        }
    }

    #[test]
    fn a_picture_with_clear_corners_keeps_its_own_shape() {
        let mut pic = shot(50, 50, None);
        for (x, y) in [(0, 0), (49, 0), (0, 49), (49, 49)] {
            pic.image.rgba[(y * 50 + x) * 4 + 3] = 0;
        }
        let p = plan(&pic, &Style::default()).unwrap();
        assert!(p.own_shape);
        assert_eq!(p.radius, 0);
        assert!(!p.shadow);
        // Asked for, they are drawn anyway.
        let asked = Style {
            radius: Some(4),
            shadow: Some(true),
            ..Style::default()
        };
        let p = plan(&pic, &asked).unwrap();
        assert_eq!(p.radius, 4);
        assert!(p.shadow);
    }

    #[test]
    fn a_radius_past_half_the_picture_is_held_there() {
        let p = plan(
            &shot(40, 20, None),
            &Style {
                radius: Some(500),
                ..Style::default()
            },
        )
        .unwrap();
        assert_eq!(p.radius, 10);
    }

    #[test]
    fn a_picture_too_large_to_draw_is_refused() {
        let e = plan(
            &shot(10, 10, None),
            &Style {
                padding: Some(15_000),
                ..Style::default()
            },
        )
        .unwrap_err();
        assert!(e.message.contains("--padding"), "{}", e.message);
    }
}
