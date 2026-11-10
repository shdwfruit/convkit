//! Pixels in and out: JPEG decoding and encoding, Lanczos resampling, Flate,
//! and the two measurements the image rules use (a JPEG's quality, read
//! from its luma table, and how alike two pictures look).

use std::io::Write;

/// How an image's samples are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Space {
    Gray,
    Rgb,
    Cmyk,
}

impl Space {
    pub(crate) fn channels(self) -> usize {
        match self {
            Space::Gray => 1,
            Space::Rgb => 3,
            Space::Cmyk => 4,
        }
    }
}

/// Decoded samples, 8 bits each, row by row.
///
/// CMYK samples are what jpeg-decoder gives: 255 minus the value stored in
/// the JPEG (for an Adobe file, the ink). jpeg-encoder stores 255 minus
/// what it is given, so a decode and an encode leave the stored values,
/// and so the source's convention, as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pixels {
    pub width: u32,
    pub height: u32,
    pub space: Space,
    pub data: Vec<u8>,
}

/// The IJG standard luminance table, in natural order. A JPEG's own luma
/// table divided by this, on average, gives the scale its quality setting
/// asked for.
const STD_LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// The marker segments before a JPEG's first scan: `(marker, payload)`.
fn segments(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return out;
    }
    let mut i = 2;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            break;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 || i + 3 >= data.len() {
            break;
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if len < 2 || i + 2 + len > data.len() {
            break;
        }
        out.push((marker, &data[i + 4..i + 2 + len]));
        i += 2 + len;
    }
    out
}

/// The transform byte of a JPEG's Adobe APP14 segment, when it has one.
pub(crate) fn adobe_transform(data: &[u8]) -> Option<u8> {
    segments(data)
        .into_iter()
        .find(|(m, p)| *m == 0xEE && p.len() >= 12 && p.starts_with(b"Adobe"))
        .map(|(_, p)| p[11])
}

/// The quality setting a JPEG was most likely written at, from its luma
/// quantisation table and the IJG scaling rule; `None` without one.
pub(crate) fn estimated_quality(data: &[u8]) -> Option<f64> {
    for (marker, mut p) in segments(data) {
        if marker != 0xDB {
            continue;
        }
        while !p.is_empty() {
            let (precision, id) = (p[0] >> 4, p[0] & 0x0F);
            let size = if precision == 0 { 64 } else { 128 };
            if p.len() < 1 + size {
                return None;
            }
            let table = &p[1..1 + size];
            if id == 0 {
                let sum: u64 = if precision == 0 {
                    table.iter().map(|&v| u64::from(v)).sum()
                } else {
                    table
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u64::from(u16::from_be_bytes([c[0], c[1]])))
                        .sum()
                };
                let std: u64 = STD_LUMA.iter().map(|&v| u64::from(v)).sum();
                let scale = 100.0 * sum as f64 / std as f64;
                return Some(if scale <= 100.0 {
                    (200.0 - scale) / 2.0
                } else {
                    5000.0 / scale
                });
            }
            p = &p[1 + size..];
        }
    }
    None
}

/// Decodes a JPEG of at most `max_pixels`. 16-bit, arithmetic-coded and
/// otherwise unsupported JPEGs are `None`.
pub(crate) fn decode_jpeg(data: &[u8], max_pixels: u64) -> Option<Pixels> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    let space = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => Space::Gray,
        jpeg_decoder::PixelFormat::RGB24 => Space::Rgb,
        jpeg_decoder::PixelFormat::CMYK32 => Space::Cmyk,
        jpeg_decoder::PixelFormat::L16 => return None,
    };
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let count = u64::from(width) * u64::from(height);
    if count == 0 || count > max_pixels {
        return None;
    }
    decoder.set_max_decoding_buffer_size(count as usize * space.channels());
    let data = decoder.decode().ok()?;
    (data.len() == count as usize * space.channels()).then_some(Pixels {
        width,
        height,
        space,
        data,
    })
}

/// Lanczos3 to `width` x `height`. CMYK's fourth channel is black, not
/// alpha, so nothing is premultiplied.
pub(crate) fn resize(p: &Pixels, width: u32, height: u32) -> Option<Pixels> {
    use fast_image_resize::pixels::{U8x3, U8x4, U8};
    let data = match p.space {
        Space::Gray => resize_as::<U8>(p, width, height),
        Space::Rgb => resize_as::<U8x3>(p, width, height),
        Space::Cmyk => resize_as::<U8x4>(p, width, height),
    }?;
    Some(Pixels {
        width,
        height,
        space: p.space,
        data,
    })
}

/// `resize` for one pixel layout. The typed call builds the resizer for
/// the three layouts conv uses only, where the untyped one would build
/// every layout the crate knows (over a megabyte of the binary).
fn resize_as<P>(p: &Pixels, width: u32, height: u32) -> Option<Vec<u8>>
where
    P: fast_image_resize::PixelTrait,
{
    use fast_image_resize::images::{TypedImage, TypedImageRef};
    use fast_image_resize::{ResizeOptions, Resizer};
    let src = TypedImageRef::<P>::from_buffer(p.width, p.height, &p.data).ok()?;
    let mut out = vec![0u8; width as usize * height as usize * p.space.channels()];
    let mut dst = TypedImage::<P>::from_buffer(width, height, &mut out).ok()?;
    Resizer::new()
        .resize_typed(&src, &mut dst, &ResizeOptions::new().use_alpha(false))
        .ok()?;
    drop(dst);
    Some(out)
}

/// Light unsharp masking of a gray picture: Gaussian blur of sigma 0.6,
/// and the difference from it added back at 0.6. Downsampling a 200 dpi
/// scan to 150 dpi turns its one-pixel lines into lighter, wider ones; this
/// brings back their darkness, as an office scan's own sharpening did.
pub(crate) fn sharpen_gray(p: &mut Pixels) {
    if p.space != Space::Gray || p.width < 3 || p.height < 3 {
        return;
    }
    const AMOUNT: f32 = 0.6;
    // A Gaussian of sigma 0.6 over five taps, normalised.
    const K: [f32; 5] = [0.0026, 0.1655, 0.6638, 0.1655, 0.0026];
    let (w, h) = (p.width as usize, p.height as usize);
    let at = |v: isize, n: usize| v.clamp(0, n as isize - 1) as usize;
    let mut across = vec![0f32; w * h];
    for y in 0..h {
        let row = &p.data[y * w..(y + 1) * w];
        for x in 0..w {
            across[y * w + x] = K
                .iter()
                .enumerate()
                .map(|(i, k)| k * f32::from(row[at(x as isize + i as isize - 2, w)]))
                .sum();
        }
    }
    for y in 0..h {
        for x in 0..w {
            let blur: f32 = K
                .iter()
                .enumerate()
                .map(|(i, k)| k * across[at(y as isize + i as isize - 2, h) * w + x])
                .sum();
            let v = f32::from(p.data[y * w + x]);
            p.data[y * w + x] = (v + AMOUNT * (v - blur)).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// A baseline JPEG at `quality`, with optimised Huffman tables. RGB is
/// written as YCbCr with 2x2 chroma below quality 90, as most encoders do;
/// CMYK as YCCK with 2x2 chroma, which halves its size against plain CMYK
/// and keeps the colours in CMYK.
pub(crate) fn encode_jpeg(p: &Pixels, quality: u8) -> Option<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let (w, h) = (u16::try_from(p.width).ok()?, u16::try_from(p.height).ok()?);
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_optimized_huffman_tables(true);
    let color = match p.space {
        Space::Gray => ColorType::Luma,
        Space::Rgb => ColorType::Rgb,
        Space::Cmyk => {
            encoder.set_sampling_factor(SamplingFactor::F_2_2);
            ColorType::CmykAsYcck
        }
    };
    encoder.encode(&p.data, w, h, color).ok()?;
    Some(out)
}

/// Samples as a zlib stream, for `/FlateDecode`. qpdf's post-pass
/// recompresses every Flate stream at level 9, so this one only has to
/// be quick.
pub(crate) fn flate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(data).expect("writing to a Vec cannot fail");
    e.finish().expect("writing to a Vec cannot fail")
}

/// Luma (BT.601) of gray or RGB pixels; CMYK is never compared.
pub(crate) fn luma(p: &Pixels) -> Vec<u8> {
    match p.space {
        Space::Gray => p.data.clone(),
        Space::Rgb => p
            .data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| {
                let y = 299 * u32::from(c[0]) + 587 * u32::from(c[1]) + 114 * u32::from(c[2]);
                ((y + 500) / 1000) as u8
            })
            .collect(),
        Space::Cmyk => Vec::new(),
    }
}

/// Mean SSIM of two luma planes of `width` x `height`, over 8x8 blocks
/// (the whole picture when it is smaller). 1.0 is identical.
pub(crate) fn ssim(a: &[u8], b: &[u8], width: u32, height: u32) -> f64 {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (w, h) = (width as usize, height as usize);
    if a.len() < w * h || b.len() < w * h || w == 0 || h == 0 {
        return 0.0;
    }
    let bw = 8.min(w);
    let bh = 8.min(h);
    let (mut total, mut blocks) = (0.0, 0u64);
    for by in (0..=h - bh).step_by(bh) {
        for bx in (0..=w - bw).step_by(bw) {
            let n = (bw * bh) as f64;
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for y in by..by + bh {
                let row = y * w;
                for x in bx..bx + bw {
                    let (va, vb) = (f64::from(a[row + x]), f64::from(b[row + x]));
                    sa += va;
                    sb += vb;
                    saa += va * va;
                    sbb += vb * vb;
                    sab += va * vb;
                }
            }
            let (ma, mb) = (sa / n, sb / n);
            let va = (saa / n - ma * ma).max(0.0);
            let vb = (sbb / n - mb * mb).max(0.0);
            let cov = sab / n - ma * mb;
            total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2))
                / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            blocks += 1;
        }
    }
    total / blocks as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn gradient(width: u32, height: u32, space: Space) -> Pixels {
        let n = space.channels();
        let mut data = Vec::with_capacity((width * height) as usize * n);
        for y in 0..height {
            for x in 0..width {
                for c in 0..n {
                    data.push(
                        ((x * 200 / width.max(1)) as usize + c * 10 + (y as usize % 7)) as u8,
                    );
                }
            }
        }
        Pixels {
            width,
            height,
            space,
            data,
        }
    }

    #[test]
    fn a_jpeg_round_trips_through_encode_and_decode() {
        for space in [Space::Gray, Space::Rgb, Space::Cmyk] {
            let p = gradient(64, 48, space);
            let jpeg = encode_jpeg(&p, 90).unwrap();
            let back = decode_jpeg(&jpeg, u64::MAX).unwrap();
            assert_eq!((back.width, back.height, back.space), (64, 48, space));
            let worst = p
                .data
                .iter()
                .zip(&back.data)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(worst < 40, "{space:?}: off by {worst}");
        }
    }

    #[test]
    fn cmyk_is_written_as_ycck_with_an_adobe_marker() {
        let jpeg = encode_jpeg(&gradient(32, 32, Space::Cmyk), 80).unwrap();
        assert_eq!(adobe_transform(&jpeg), Some(2));
        assert_eq!(
            adobe_transform(&encode_jpeg(&gradient(32, 32, Space::Rgb), 80).unwrap()),
            None
        );
    }

    #[test]
    fn cmyk_keeps_its_stored_values_through_a_decode_and_an_encode() {
        // What a decoder that does not invert (libjpeg, and so poppler)
        // reads from the file must survive: jpeg-decoder hands back
        // 255 - stored, jpeg-encoder stores 255 - given.
        let ink = gradient(40, 40, Space::Cmyk);
        let first = encode_jpeg(&ink, 95).unwrap();
        let decoded = decode_jpeg(&first, u64::MAX).unwrap();
        let second = encode_jpeg(&decoded, 95).unwrap();
        let again = decode_jpeg(&second, u64::MAX).unwrap();
        let worst = decoded
            .data
            .iter()
            .zip(&again.data)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst < 24, "off by {worst}");
        let mean_ink: f64 =
            ink.data.iter().map(|&v| f64::from(v)).sum::<f64>() / ink.data.len() as f64;
        let mean_back: f64 =
            again.data.iter().map(|&v| f64::from(v)).sum::<f64>() / again.data.len() as f64;
        assert!(
            (mean_ink - mean_back).abs() < 3.0,
            "{mean_ink} vs {mean_back}"
        );
    }

    #[test]
    fn quality_is_read_back_from_the_luma_table() {
        for q in [40u8, 65, 75, 80, 92] {
            let jpeg = encode_jpeg(&gradient(16, 16, Space::Gray), q).unwrap();
            let est = estimated_quality(&jpeg).unwrap();
            assert!((est - f64::from(q)).abs() <= 3.0, "q{q} read as {est}");
        }
        assert_eq!(estimated_quality(b"not a jpeg"), None);
    }

    #[test]
    fn resizing_keeps_a_flat_picture_flat() {
        for space in [Space::Gray, Space::Rgb, Space::Cmyk] {
            let n = space.channels();
            let p = Pixels {
                width: 30,
                height: 20,
                space,
                data: vec![200; 600 * n],
            };
            let small = resize(&p, 15, 10).unwrap();
            assert_eq!(small.data.len(), 150 * n);
            assert!(
                small.data.iter().all(|&v| v.abs_diff(200) <= 1),
                "{space:?}"
            );
        }
    }

    #[test]
    fn sharpening_darkens_a_softened_line_and_leaves_flat_gray_alone() {
        // A one-pixel line softened across three pixels.
        let (w, h) = (9u32, 5u32);
        let mut data = vec![240u8; (w * h) as usize];
        for y in 0..h as usize {
            data[y * 9 + 3] = 200;
            data[y * 9 + 4] = 120;
            data[y * 9 + 5] = 200;
        }
        let mut p = Pixels {
            width: w,
            height: h,
            space: Space::Gray,
            data,
        };
        sharpen_gray(&mut p);
        assert!(
            p.data[2 * 9 + 4] < 120,
            "the line is darker: {}",
            p.data[2 * 9 + 4]
        );
        assert_eq!(p.data[2 * 9], 240, "flat paper stays as it was");
        let mut rgb = gradient(8, 8, Space::Rgb);
        let before = rgb.clone();
        sharpen_gray(&mut rgb);
        assert_eq!(rgb, before, "only gray is sharpened");
    }

    #[test]
    fn ssim_is_one_for_the_same_picture_and_lower_for_noise() {
        let p = gradient(64, 64, Space::Gray);
        assert!((ssim(&p.data, &p.data, 64, 64) - 1.0).abs() < 1e-9);
        let noisy: Vec<u8> = p
            .data
            .iter()
            .enumerate()
            .map(|(i, &v)| if i % 3 == 0 { v.wrapping_add(90) } else { v })
            .collect();
        assert!(ssim(&p.data, &noisy, 64, 64) < 0.8);
        assert!(
            ssim(&[1, 2, 3], &[1, 2, 3], 3, 1) > 0.99,
            "smaller than a block"
        );
    }

    #[test]
    fn luma_weights_green_most() {
        let p = Pixels {
            width: 3,
            height: 1,
            space: Space::Rgb,
            data: vec![255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        assert_eq!(luma(&p), vec![76, 150, 29]);
    }

    #[test]
    fn an_oversized_or_empty_jpeg_is_not_decoded() {
        let jpeg = encode_jpeg(&gradient(64, 64, Space::Rgb), 80).unwrap();
        assert!(decode_jpeg(&jpeg, 100).is_none(), "over the pixel limit");
        assert!(decode_jpeg(b"\xFF\xD8\xFF\xD9", u64::MAX).is_none());
    }
}
