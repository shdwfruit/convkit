//! Reading the QR codes in a picture (`conv qr --read`), with rqrr, the
//! decoder `conv qr` already checks its own drawings with.
//!
//! A screenshot reads on the first try. A phone photo often doesn't: the
//! code is small in the frame, tilted, soft, unevenly lit or low in
//! contrast, and JPEG noise breaks up its edges. So the picture is read
//! again, a pass at a time, as other versions of itself: smaller (which
//! averages noise and blur away), contrast-stretched, blurred, thresholded
//! as a whole, light-on-dark turned around, and, for a tiny picture,
//! larger. Reading stops at the first pass after which nothing is left
//! half-read: every code found so far decoded, and none seen that didn't.
//! Then a code seen but never read is read again from just its part of the
//! picture, and a large picture where nothing was seen is read again a
//! quarter at a time, for a code too small in the frame to be seen whole.
//!
//! Nothing in a code is acted on: a link is text to print, never opened.

use serde::Serialize;

use crate::ascii::{self, Image};
use crate::{ConvError, ErrorCode, Format, Result};

/// The longest side the first pass reads at. A larger picture is box
/// averaged down to this, which costs a small code nothing (a module is
/// still several pixels) and keeps a 48 MP photo from taking seconds.
const FULL_SIDE: u32 = 4096;

/// An SVG is drawn this many pixels on its longer side to be read.
const SVG_SIDE: u32 = 2048;

/// One code found in a picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Code {
    /// What the code holds. A code's bytes are read as UTF-8, or as
    /// ISO 8859-1 (the standard's own default) when they are not UTF-8.
    pub text: String,
    /// The QR version, 1 (21 modules across) to 40 (177).
    pub version: usize,
    /// The code's corners in the picture's pixels: top-left, top-right,
    /// bottom-right, bottom-left, as the code itself is turned.
    pub corners: [[i32; 2]; 4],
    /// The network a Wi-Fi code joins, read from its `WIFI:` text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi: Option<Wifi>,
}

/// A Wi-Fi code's network: `WIFI:S:<name>;T:<security>;P:<password>;;`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wifi {
    /// The network's name (SSID).
    pub network: String,
    /// `WPA`, `WEP`, `nopass` (an open network), or whatever else the
    /// code says (`SAE`, `WPA2-EAP`). A code without `T:` is open.
    pub security: String,
    pub password: Option<String>,
    /// Whether the network hides its name (`H:true`).
    pub hidden: bool,
}

/// A picture as luminance, light 255, ready to be read.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Each pixel over white.
    luma: Vec<u8>,
    /// For a picture with see-through pixels, each pixel over black and
    /// then light and dark swapped: a light code on a clear background
    /// vanishes over white, and reads as a dark one this way.
    over_black: Option<Vec<u8>>,
}

impl Picture {
    /// A picture from 8-bit RGBA, not premultiplied.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Picture {
        let lum = |p: &[u8]| (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000;
        let clear = rgba.chunks_exact(4).any(|p| p[3] < 255);
        let luma = rgba
            .chunks_exact(4)
            .map(|p| {
                let a = p[3] as u32;
                ((lum(p) * a + 255 * (255 - a)) / 255) as u8
            })
            .collect();
        let over_black = clear.then(|| {
            rgba.chunks_exact(4)
                .map(|p| 255 - (lum(p) * p[3] as u32 / 255) as u8)
                .collect()
        });
        Picture {
            width,
            height,
            luma,
            over_black,
        }
    }

    /// A picture from 8-bit luminance.
    pub fn from_luma(width: u32, height: u32, luma: Vec<u8>) -> Picture {
        Picture {
            width,
            height,
            luma,
            over_black: None,
        }
    }

    fn from_image(img: &Image) -> Picture {
        Picture::from_rgba(img.width, img.height, &img.rgba)
    }
}

/// Whether conv decodes `format` itself, with no backend.
pub fn decodes_itself(format: Format) -> bool {
    matches!(
        format,
        Format::Png | Format::Jpg | Format::Gif | Format::Webp | Format::Svg
    )
}

fn unreadable(what: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("could not read the picture: {what}"),
    )
}

/// Decodes a picture conv reads itself (see `decodes_itself`). A GIF gives
/// its first frame.
pub fn decode(bytes: &[u8], format: Format) -> Result<Picture> {
    match format {
        Format::Png => Ok(Picture::from_image(&ascii::decode_png(bytes)?)),
        Format::Svg => Ok(Picture::from_image(&ascii::from_svg(bytes, SVG_SIDE)?)),
        Format::Jpg => decode_jpeg(bytes),
        Format::Gif => decode_gif(bytes),
        Format::Webp => decode_webp(bytes),
        other => Err(unreadable(format!(
            "conv does not decode .{} itself",
            other.ext()
        ))),
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<Picture> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let luma = decoder.decode().map_err(unreadable)?;
    let info = decoder.info().ok_or_else(|| unreadable("no JPEG header"))?;
    let (w, h) = (info.width as u32, info.height as u32);
    if decoder.output_colorspace() != Some(ColorSpace::Luma) || luma.len() != (w * h) as usize {
        return Err(unreadable("an unexpected JPEG colour layout"));
    }
    Ok(Picture::from_luma(w, h, luma))
}

fn decode_gif(bytes: &[u8]) -> Result<Picture> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(bytes).map_err(unreadable)?;
    let frame = decoder
        .read_next_frame()
        .map_err(unreadable)?
        .ok_or_else(|| unreadable("the GIF has no frames"))?;
    Ok(Picture::from_rgba(
        frame.width as u32,
        frame.height as u32,
        &frame.buffer,
    ))
}

fn decode_webp(bytes: &[u8]) -> Result<Picture> {
    let mut decoder =
        image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(unreadable)?;
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| unreadable("it is too large"))?;
    let mut buf = vec![0; size];
    decoder.read_image(&mut buf).map_err(unreadable)?;
    let (w, h) = decoder.dimensions();
    if decoder.has_alpha() {
        Ok(Picture::from_rgba(w, h, &buf))
    } else {
        let rgba: Vec<u8> = buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        Ok(Picture::from_rgba(w, h, &rgba))
    }
}

/// A grey picture being read, and how far its pixels are from the
/// original's: an original pixel is `scale` of these.
struct Grey {
    w: usize,
    h: usize,
    px: Vec<u8>,
    scale: f64,
}

/// One way of reading the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pass {
    /// As it is, no longer than this on its longer side.
    Plain(u32),
    /// Its darkest percent black and lightest white.
    Stretched(u32),
    /// Softened, so noise and halftone dots run together.
    Blurred(u32),
    /// Black and white at one threshold for the whole picture (Otsu's),
    /// instead of rqrr's own, which follows the light along each row.
    Threshold(u32),
    /// Light and dark swapped, for a light code on a dark background.
    Inverted(u32),
    /// Twice the size, for a tiny picture.
    Doubled,
    /// See-through pixels over black instead of white, light and dark
    /// swapped.
    OverBlack,
}

/// The passes, in order. Cheap and likely first.
fn passes(pic: &Picture) -> Vec<Pass> {
    let mut v = vec![
        Pass::Plain(FULL_SIDE),
        Pass::Plain(1600),
        Pass::Plain(800),
        Pass::Stretched(FULL_SIDE),
        Pass::Blurred(FULL_SIDE),
        Pass::Threshold(1600),
        Pass::Plain(400),
        Pass::Inverted(FULL_SIDE),
    ];
    if pic.width.max(pic.height) < 1000 {
        v.push(Pass::Doubled);
    }
    if pic.over_black.is_some() {
        v.push(Pass::OverBlack);
    }
    // A small picture is the same at several of these sides; read it once.
    let long = pic.width.max(pic.height);
    let mut seen: Vec<(std::mem::Discriminant<Pass>, u32)> = Vec::new();
    v.retain(|p| {
        let side = match *p {
            Pass::Plain(s)
            | Pass::Stretched(s)
            | Pass::Blurred(s)
            | Pass::Threshold(s)
            | Pass::Inverted(s) => long.div_ceil(long.div_ceil(s)),
            Pass::Doubled | Pass::OverBlack => 0,
        };
        let key = (std::mem::discriminant(p), side);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    v
}

/// Every QR code in the picture, in reading order: rows top to bottom,
/// each left to right. A code that holds the same text as one already
/// found is not listed again.
///
/// After the passes over the whole picture, a code that was seen but not
/// read gets a closer look: the passes again, over just that part of the
/// picture, where a small picture is also read at twice its size and
/// rqrr's threshold follows the light over a shorter stretch. A large
/// picture where nothing was seen at all is read again in nine
/// overlapping quarters, for a code too small in the frame to be seen.
pub fn read(pic: &Picture) -> Vec<Code> {
    let (mut found, missed) = read_passes(pic, false);
    look_closer(pic, &mut found, &missed);
    if found.is_empty() && pic.width.max(pic.height) >= 1000 {
        read_quarters(pic, &mut found);
    }
    in_reading_order(found)
}

/// The codes in rows, top to bottom, and each row left to right. A code
/// starts a new row when its top is below the bottom of every code in the
/// row so far.
fn in_reading_order(mut codes: Vec<Code>) -> Vec<Code> {
    let top = |c: &Code| c.corners.iter().map(|p| p[1]).min().unwrap();
    let bottom = |c: &Code| c.corners.iter().map(|p| p[1]).max().unwrap();
    codes.sort_by_key(top);
    let mut rows: Vec<(i32, Vec<Code>)> = Vec::new();
    for code in codes {
        match rows.last_mut() {
            Some((end, row)) if top(&code) < *end => {
                *end = (*end).max(bottom(&code));
                row.push(code);
            }
            _ => rows.push((bottom(&code), vec![code])),
        }
    }
    rows.into_iter()
        .flat_map(|(_, mut row)| {
            row.sort_by_key(|c| centre(&c.corners).0);
            row
        })
        .collect()
}

/// Reads again, closer, around each code seen but not read.
fn look_closer(pic: &Picture, found: &mut Vec<Code>, missed: &[[[i32; 2]; 4]]) {
    for region in missed {
        let (cx, cy) = centre(region);
        if found.iter().any(|c| inside(&c.corners, cx, cy)) {
            continue;
        }
        let xs = region.map(|p| p[0]);
        let ys = region.map(|p| p[1]);
        let (x0, x1) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
        let (y0, y1) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
        let margin = (x1 - x0).max(y1 - y0) / 2;
        let part = crop(pic, x0 - margin, y0 - margin, x1 + margin, y1 + margin);
        merge(found, part.map(|(p, at)| (read_passes(&p, false).0, at)));
    }
}

/// Reads nine overlapping quarters of the picture, side by side.
fn read_quarters(pic: &Picture, found: &mut Vec<Code>) {
    let (w, h) = (pic.width as i32, pic.height as i32);
    let parts: Vec<_> = (0..9)
        .filter_map(|i| {
            let (x0, y0) = (i % 3 * w / 4, i / 3 * h / 4);
            crop(pic, x0, y0, x0 + w / 2, y0 + h / 2)
        })
        .collect();
    let read_parts: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = parts
            .iter()
            .map(|(part, at)| s.spawn(move || (read_passes(part, true).0, *at)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for part in read_parts {
        merge(found, Some(part));
    }
}

/// Adds the codes read in a part of the picture that starts at `at`.
fn merge(found: &mut Vec<Code>, part: Option<(Vec<Code>, [i32; 2])>) {
    let Some((codes, [dx, dy])) = part else {
        return;
    };
    for mut code in codes {
        if !found.iter().any(|c| c.text == code.text) {
            code.corners = code.corners.map(|[x, y]| [x + dx, y + dy]);
            found.push(code);
        }
    }
}

fn centre(corners: &[[i32; 2]; 4]) -> (i32, i32) {
    (
        corners.iter().map(|p| p[0]).sum::<i32>() / 4,
        corners.iter().map(|p| p[1]).sum::<i32>() / 4,
    )
}

/// Whether (`x`, `y`) is inside the box around `corners`.
fn inside(corners: &[[i32; 2]; 4], x: i32, y: i32) -> bool {
    let xs = corners.map(|p| p[0]);
    let ys = corners.map(|p| p[1]);
    (*xs.iter().min().unwrap()..=*xs.iter().max().unwrap()).contains(&x)
        && (*ys.iter().min().unwrap()..=*ys.iter().max().unwrap()).contains(&y)
}

/// The part of the picture from (`x0`, `y0`) to (`x1`, `y1`), cut to fit
/// inside it, and where it starts; `None` when that leaves too little to
/// hold a code.
fn crop(pic: &Picture, x0: i32, y0: i32, x1: i32, y1: i32) -> Option<(Picture, [i32; 2])> {
    let (w, h) = (pic.width as i32, pic.height as i32);
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(w), y1.min(h));
    if x1 - x0 < 21 || y1 - y0 < 21 {
        return None;
    }
    let cut = |luma: &[u8]| -> Vec<u8> {
        (y0..y1)
            .flat_map(|y| &luma[(y * w + x0) as usize..(y * w + x1) as usize])
            .copied()
            .collect()
    };
    Some((
        Picture {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
            luma: cut(&pic.luma),
            over_black: pic.over_black.as_deref().map(cut),
        },
        [x0, y0],
    ))
}

/// The passes over the whole of `pic`, until one leaves nothing half-read:
/// the codes read, and where each code seen but never read is. A `part` of
/// a picture nothing was seen in gets only the passes that most often find
/// something there.
fn read_passes(pic: &Picture, part: bool) -> (Vec<Code>, Vec<[[i32; 2]; 4]>) {
    let mut found: Vec<Code> = Vec::new();
    let mut missed: Vec<[[i32; 2]; 4]> = Vec::new();
    let mut passes = passes(pic);
    if part {
        passes.retain(|p| {
            matches!(
                p,
                Pass::Plain(FULL_SIDE)
                    | Pass::Stretched(FULL_SIDE)
                    | Pass::Blurred(FULL_SIDE)
                    | Pass::Threshold(1600)
                    | Pass::Doubled
            )
        });
    }
    for pass in passes {
        let (codes, seen) = scan(pic, pass);
        for code in codes {
            if !found.iter().any(|c| c.text == code.text) {
                found.push(code);
            }
        }
        let unread = !seen.is_empty();
        for region in seen {
            let (cx, cy) = centre(&region);
            if !missed.iter().any(|m| inside(m, cx, cy)) {
                missed.push(region);
            }
        }
        if !found.is_empty() && !unread {
            break;
        }
    }
    missed.retain(|m| {
        let (cx, cy) = centre(m);
        !found.iter().any(|c| inside(&c.corners, cx, cy))
    });
    (found, missed)
}

/// The codes one pass reads, and where it saw codes it could not read.
fn scan(pic: &Picture, pass: Pass) -> (Vec<Code>, Vec<[[i32; 2]; 4]>) {
    let (grey, threshold) = match pass {
        Pass::Plain(s) => (shrink(pic, &pic.luma, s), None),
        Pass::Stretched(s) => (stretch(shrink(pic, &pic.luma, s)), None),
        Pass::Blurred(s) => (blur(shrink(pic, &pic.luma, s)), None),
        Pass::Inverted(s) => {
            let mut g = shrink(pic, &pic.luma, s);
            g.px.iter_mut().for_each(|v| *v = 255 - *v);
            (g, None)
        }
        Pass::Doubled => (double(pic), None),
        Pass::OverBlack => match &pic.over_black {
            Some(luma) => (shrink(pic, luma, FULL_SIDE), None),
            None => return (vec![], vec![]),
        },
        Pass::Threshold(s) => {
            let g = shrink(pic, &pic.luma, s);
            let t = otsu(&g.px);
            (g, Some(t))
        }
    };
    let Grey { w, h, px, scale } = grey;
    let mut prepared = match threshold {
        None => rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| px[y * w + x]),
        Some(t) => rqrr::PreparedImage::prepare_from_bitmap(w, h, |x, y| px[y * w + x] <= t),
    };
    let mut codes = Vec::new();
    let mut missed = Vec::new();
    for grid in prepared.detect_grids() {
        let corners = grid.bounds.map(|p| {
            [
                (p.x as f64 * scale).round() as i32,
                (p.y as f64 * scale).round() as i32,
            ]
        });
        let mut bytes = Vec::new();
        match grid.decode_to(&mut bytes) {
            Ok(meta) => {
                let text = match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
                };
                codes.push(Code {
                    wifi: parse_wifi(&text),
                    text,
                    version: meta.version.0,
                    corners,
                });
            }
            Err(_) => missed.push(corners),
        }
    }
    (codes, missed)
}

/// `luma` averaged down by a whole factor until its longer side is at most
/// `side`, or as it is when it already is.
fn shrink(pic: &Picture, luma: &[u8], side: u32) -> Grey {
    let (w, h) = (pic.width as usize, pic.height as usize);
    let k = (w.max(h)).div_ceil(side as usize).max(1);
    if k == 1 {
        return Grey {
            w,
            h,
            px: luma.to_vec(),
            scale: 1.0,
        };
    }
    let (sw, sh) = (w / k, h / k);
    let mut px = Vec::with_capacity(sw * sh);
    for y in 0..sh {
        for x in 0..sw {
            let mut sum = 0u32;
            for yy in y * k..(y + 1) * k {
                sum += luma[yy * w + x * k..yy * w + (x + 1) * k]
                    .iter()
                    .map(|&v| v as u32)
                    .sum::<u32>();
            }
            px.push((sum / (k * k) as u32) as u8);
        }
    }
    Grey {
        w: sw,
        h: sh,
        px,
        scale: k as f64,
    }
}

/// The picture at twice its size, each new pixel the average of the
/// nearest original ones.
fn double(pic: &Picture) -> Grey {
    let (w, h) = (pic.width as usize, pic.height as usize);
    let (dw, dh) = (w * 2, h * 2);
    let at = |x: usize, y: usize| pic.luma[y.min(h - 1) * w + x.min(w - 1)] as u32;
    let mut px = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        for x in 0..dw {
            let (sx, sy) = (x / 2, y / 2);
            // The other neighbour on each axis is toward this half-pixel.
            let nx = if x % 2 == 0 {
                sx.saturating_sub(1)
            } else {
                sx + 1
            };
            let ny = if y % 2 == 0 {
                sy.saturating_sub(1)
            } else {
                sy + 1
            };
            let v = (9 * at(sx, sy) + 3 * at(nx, sy) + 3 * at(sx, ny) + at(nx, ny)) / 16;
            px.push(v as u8);
        }
    }
    Grey {
        w: dw,
        h: dh,
        px,
        scale: 0.5,
    }
}

/// The darkest 1% made black and the lightest 1% white, the rest spread
/// evenly between.
fn stretch(mut g: Grey) -> Grey {
    let mut hist = [0usize; 256];
    g.px.iter().for_each(|&v| hist[v as usize] += 1);
    let cut = g.px.len() / 100;
    let mut acc = 0;
    let lo = (0..256).find(|&i| {
        acc += hist[i];
        acc > cut
    });
    acc = 0;
    let hi = (0..256).rev().find(|&i| {
        acc += hist[i];
        acc > cut
    });
    let (Some(lo), Some(hi)) = (lo, hi) else {
        return g;
    };
    if hi <= lo + 1 {
        return g;
    }
    let span = (hi - lo) as u32;
    g.px.iter_mut().for_each(|v| {
        *v = ((v.saturating_sub(lo as u8) as u32 * 255) / span).min(255) as u8;
    });
    g
}

/// A box blur, as wide as about a thousandth of the picture's longer side
/// each way, and at least a pixel.
fn blur(g: Grey) -> Grey {
    let r = (g.w.max(g.h) / 1000).max(1);
    let pass = |src: &[u8], w: usize, h: usize, horizontal: bool| -> Vec<u8> {
        let mut out = vec![0u8; src.len()];
        let (len, lines) = if horizontal { (w, h) } else { (h, w) };
        let idx = |line: usize, i: usize| {
            if horizontal {
                line * w + i
            } else {
                i * w + line
            }
        };
        for line in 0..lines {
            for i in 0..len {
                let (a, b) = (i.saturating_sub(r), (i + r).min(len - 1));
                let sum: u32 = (a..=b).map(|j| src[idx(line, j)] as u32).sum();
                out[idx(line, i)] = (sum / (b - a + 1) as u32) as u8;
            }
        }
        out
    };
    let px = pass(&g.px, g.w, g.h, true);
    let px = pass(&px, g.w, g.h, false);
    Grey { px, ..g }
}

/// Otsu's threshold: the grey level that best splits the picture's pixels
/// into a dark group and a light one.
fn otsu(px: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    px.iter().for_each(|&v| hist[v as usize] += 1);
    let total = px.len() as f64;
    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &n)| i as f64 * n as f64)
        .sum();
    let (mut w0, mut sum0, mut best, mut best_t) = (0f64, 0f64, -1f64, 127u8);
    for (t, &n) in hist.iter().enumerate() {
        w0 += n as f64;
        if w0 == 0.0 {
            continue;
        }
        let w1 = total - w0;
        if w1 == 0.0 {
            break;
        }
        sum0 += t as f64 * n as f64;
        let (m0, m1) = (sum0 / w0, (sum - sum0) / w1);
        let between = w0 * w1 * (m0 - m1) * (m0 - m1);
        if between > best {
            best = between;
            best_t = t as u8;
        }
    }
    best_t
}

/// The network in a Wi-Fi code's text, or `None` for any other text.
/// Fields come in any order; `\` escapes `\ ; , : "` inside a value.
pub fn parse_wifi(text: &str) -> Option<Wifi> {
    let rest = text
        .get(..5)
        .filter(|p| p.eq_ignore_ascii_case("WIFI:"))
        .map(|_| &text[5..])?;
    let (mut network, mut security, mut password, mut hidden) = (None, None, None, false);
    let mut chars = rest.chars().peekable();
    loop {
        // A field's key runs to its ':'.
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == ':' {
                break;
            }
            key.push(c);
        }
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        let mut quoted = (false, false);
        let mut first = true;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        value.push(n);
                    }
                    quoted.1 = false;
                }
                ';' => break,
                '"' => {
                    if first {
                        quoted.0 = true;
                    }
                    quoted.1 = true;
                    value.push(c);
                }
                _ => {
                    quoted.1 = false;
                    value.push(c);
                }
            }
            first = false;
        }
        // Some generators put a name that could pass for hex in quotes.
        if quoted.0 && quoted.1 && value.len() >= 2 {
            value = value[1..value.len() - 1].to_string();
        }
        match key.to_ascii_uppercase().as_str() {
            "S" => network = Some(value),
            "T" => security = Some(value),
            "P" => password = Some(value),
            "H" => hidden = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
        if chars.peek().is_none() {
            break;
        }
    }
    let security = security
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nopass".to_string());
    let password = password.filter(|p| !p.is_empty() && security != "nopass");
    Some(Wifi {
        network: network?,
        security,
        password,
        hidden,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qr::make;

    /// The code `conv qr` makes for `text`, `size` pixels square, as a
    /// picture.
    fn made(text: &str, size: u32) -> Picture {
        let png = make(text, None, size).unwrap().png().unwrap();
        decode(&png, Format::Png).unwrap()
    }

    fn texts(pic: &Picture) -> Vec<String> {
        read(pic).into_iter().map(|c| c.text).collect()
    }

    /// `pic` turned a quarter `turns` times clockwise.
    fn turned(pic: &Picture, turns: u32) -> Picture {
        let mut p = pic.clone();
        for _ in 0..turns {
            let (w, h) = (p.width as usize, p.height as usize);
            let mut out = vec![0; w * h];
            for y in 0..h {
                for x in 0..w {
                    out[x * h + (h - 1 - y)] = p.luma[y * w + x];
                }
            }
            p = Picture::from_luma(h as u32, w as u32, out);
        }
        p
    }

    /// `pic` on a light grey `w` x `h` canvas at (`x`, `y`).
    fn placed(pic: &Picture, w: u32, h: u32, x: u32, y: u32) -> Picture {
        let mut luma = vec![200u8; (w * h) as usize];
        for row in 0..pic.height {
            for col in 0..pic.width {
                luma[((y + row) * w + x + col) as usize] =
                    pic.luma[(row * pic.width + col) as usize];
            }
        }
        Picture::from_luma(w, h, luma)
    }

    #[test]
    fn its_own_codes_read_at_many_sizes() {
        for size in [29 * 2, 29 * 3, 200, 600, 1024, 3000] {
            assert_eq!(texts(&made("hi", size)), ["hi"], "at {size}px");
        }
        let link = "https://example.com/menu?table=4";
        for size in [120, 400, 1024] {
            assert_eq!(texts(&made(link, size)), [link], "at {size}px");
        }
    }

    #[test]
    fn its_own_codes_read_turned_any_way() {
        let pic = made("https://example.com/turned", 400);
        for turns in 1..4 {
            assert_eq!(
                texts(&turned(&pic, turns)),
                ["https://example.com/turned"],
                "turned {turns} quarters"
            );
        }
    }

    #[test]
    fn a_big_code_reads_back_whole() {
        let text = "x".repeat(2331);
        assert_eq!(texts(&made(&text, 2000)), [text]);
    }

    #[test]
    fn every_code_in_a_picture_reads_top_to_bottom() {
        let a = made("first", 300);
        let b = made("second", 300);
        let mut pic = placed(&b, 900, 900, 500, 520);
        // Paste `a` above it.
        for row in 0..a.height {
            for col in 0..a.width {
                pic.luma[((40 + row) * 900 + 60 + col) as usize] =
                    a.luma[(row * a.width + col) as usize];
            }
        }
        let codes = read(&pic);
        assert_eq!(
            codes.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        // Corners are in the picture's own pixels.
        let [x, y] = codes[0].corners[0];
        assert!((40..110).contains(&x) && (20..90).contains(&y), "{x},{y}");
    }

    #[test]
    fn codes_side_by_side_read_left_to_right() {
        let a = made("left", 300);
        let b = made("right", 300);
        // `right` sits a little higher, but in the same row.
        let mut pic = placed(&b, 900, 500, 520, 60);
        for row in 0..a.height {
            for col in 0..a.width {
                pic.luma[((120 + row) * 900 + 40 + col) as usize] =
                    a.luma[(row * a.width + col) as usize];
            }
        }
        assert_eq!(texts(&pic), ["left", "right"]);
    }

    #[test]
    fn a_small_code_in_a_big_picture_reads() {
        let pic = placed(&made("small", 116), 3000, 2000, 2100, 1500);
        assert_eq!(texts(&pic), ["small"]);
    }

    #[test]
    fn a_faint_code_reads() {
        let mut pic = made("faint", 300);
        pic.luma.iter_mut().for_each(|v| *v = 150 + *v / 8);
        assert_eq!(texts(&pic), ["faint"]);
    }

    #[test]
    fn a_light_code_on_dark_reads() {
        let mut pic = made("inverted", 300);
        pic.luma.iter_mut().for_each(|v| *v = 255 - *v);
        assert_eq!(texts(&pic), ["inverted"]);
    }

    #[test]
    fn a_light_code_on_a_clear_background_reads() {
        let code = made("clear", 290);
        // White modules, everything else see-through.
        let rgba: Vec<u8> = code
            .luma
            .iter()
            .flat_map(|&v| {
                if v > 128 {
                    [0, 0, 0, 0]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        assert_eq!(
            texts(&Picture::from_rgba(code.width, code.height, &rgba)),
            ["clear"]
        );
    }

    #[test]
    fn a_picture_without_a_code_reads_nothing() {
        let w = 300usize;
        let luma = (0..w * w).map(|i| ((i % w) * 255 / w) as u8).collect();
        assert!(read(&Picture::from_luma(w as u32, w as u32, luma)).is_empty());
    }

    #[test]
    fn non_ascii_text_reads_unchanged() {
        let text = "日本語のメニュー";
        assert_eq!(texts(&made(text, 400)), [text]);
    }

    #[test]
    fn wifi_codes_are_parsed() {
        let w = parse_wifi("WIFI:S:Home;T:WPA;P:secret;;").unwrap();
        assert_eq!(
            w,
            Wifi {
                network: "Home".into(),
                security: "WPA".into(),
                password: Some("secret".into()),
                hidden: false,
            }
        );
        let code = read(&made("WIFI:S:Home;T:WPA;P:secret;;", 400));
        assert_eq!(code[0].wifi.as_ref(), Some(&w));
    }

    #[test]
    fn wifi_fields_come_in_any_order_with_escapes() {
        let w = parse_wifi(r#"WIFI:T:WEP;P:a\;b\:c\\d;S:Caf\,e "x";H:true;;"#).unwrap();
        assert_eq!(w.network, "Caf,e \"x\"");
        assert_eq!(w.security, "WEP");
        assert_eq!(w.password.as_deref(), Some("a;b:c\\d"));
        assert!(w.hidden);
    }

    #[test]
    fn an_open_wifi_network_has_no_password() {
        let w = parse_wifi("WIFI:S:Guest;;").unwrap();
        assert_eq!(w.security, "nopass");
        assert_eq!(w.password, None);
        let w = parse_wifi("wifi:S:\"012345\";T:nopass;P:;;").unwrap();
        assert_eq!(w.network, "012345");
        assert_eq!(w.password, None);
    }

    #[test]
    fn other_text_is_not_wifi() {
        assert_eq!(parse_wifi("https://example.com"), None);
        assert_eq!(parse_wifi("WIFI:T:WPA;;"), None);
        assert_eq!(parse_wifi("WIFI"), None);
        assert_eq!(parse_wifi("WIFI:é"), None);
    }

    /// Phone-style photos: one of conv's own codes, tilted in perspective
    /// and small in a 1200x900 frame, under uneven light, then blurred,
    /// low in contrast, noisy and saved as a rough JPEG. Each was made with
    /// ImageMagick from `conv qr "https://example.com/menu?table=4" --size
    /// 600`: `-distort Perspective` on the code, composited at 45% onto
    /// `plasma:` (blurred, desaturated), multiplied by a tilted gradient,
    /// `+level 22%,78% -gaussian-blur 0xB -attenuate N +noise Gaussian`,
    /// resized to 1200x900 and saved at `-quality 55`; B and N are 2 and 1
    /// for qr-phone.jpg, 1.8 and 0.7 for qr-phone-soft.jpg.
    fn phone_photo(name: &str) -> Picture {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        decode(&std::fs::read(path).unwrap(), Format::Jpg).unwrap()
    }

    const PHONE_LINK: &str = "https://example.com/menu?table=4";

    #[test]
    fn a_phone_photo_reads() {
        assert_eq!(texts(&phone_photo("qr-phone.jpg")), [PHONE_LINK]);
        assert_eq!(texts(&phone_photo("qr-phone-soft.jpg")), [PHONE_LINK]);
    }

    #[test]
    fn a_code_too_small_to_see_is_found_in_a_quarter() {
        let pic = phone_photo("qr-phone.jpg");
        // No pass over the whole picture sees it at all.
        assert!(scan(&pic, Pass::Plain(FULL_SIDE)).0.is_empty());
        assert_eq!(read_passes(&pic, false), (vec![], vec![]));
        let mut found = Vec::new();
        read_quarters(&pic, &mut found);
        assert_eq!(found.len(), 1);
        // Its corners are in the whole picture's pixels: the code sits
        // around (780, 500).
        let (x, y) = centre(&found[0].corners);
        assert!(
            (700..860).contains(&x) && (420..580).contains(&y),
            "{x},{y}"
        );
    }

    #[test]
    fn a_code_seen_but_not_read_gets_a_closer_look() {
        let pic = phone_photo("qr-phone-soft.jpg");
        let (codes, missed) = read_passes(&pic, false);
        assert!(codes.is_empty() && !missed.is_empty());
        let mut found = Vec::new();
        look_closer(&pic, &mut found, &missed);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, PHONE_LINK);
    }

    #[test]
    fn a_jpeg_png_gif_and_webp_all_decode() {
        // A 2x1 picture, black then white, in each format conv decodes.
        let png = {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, 2, 1);
            e.set_color(png::ColorType::Grayscale);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[0, 255]).unwrap();
            w.finish().unwrap();
            out
        };
        let p = decode(&png, Format::Png).unwrap();
        assert_eq!((p.width, p.height, p.luma.clone()), (2, 1, vec![0, 255]));
        let gif = {
            let mut out = Vec::new();
            {
                let mut e = gif::Encoder::new(&mut out, 2, 1, &[0, 0, 0, 255, 255, 255]).unwrap();
                let f = gif::Frame {
                    width: 2,
                    height: 1,
                    buffer: std::borrow::Cow::Owned(vec![0, 1]),
                    ..Default::default()
                };
                e.write_frame(&f).unwrap();
            }
            out
        };
        let p = decode(&gif, Format::Gif).unwrap();
        assert_eq!(p.luma, vec![0, 255]);
        let webp = {
            let mut out = Vec::new();
            image_webp::WebPEncoder::new(&mut out)
                .encode(&[0, 0, 0, 255, 255, 255], 2, 1, image_webp::ColorType::Rgb8)
                .unwrap();
            out
        };
        let p = decode(&webp, Format::Webp).unwrap();
        assert_eq!(p.luma, vec![0, 255]);
        assert!(decode(b"not a jpeg", Format::Jpg).is_err());
        assert!(decode(b"", Format::Heic).is_err());
    }
}
