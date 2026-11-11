//! Pictures conv decodes itself, upright, for the commands that look at
//! their pixels: `conv colors` and `conv beautify`.
//!
//! PNG, JPEG, GIF, WebP and SVG are decoded here with the decoders resvg
//! already brings in, so a screenshot or a phone photo needs no backend.
//! Every other picture goes through ImageMagick to a PNG first, which the
//! caller arranges. A JPEG, PNG or WebP turned by its EXIF orientation tag
//! comes out the way it is shown, so a pixel's position is the one a viewer
//! shows it at. What a re-encode needs to look the same is kept too: the
//! colour profile, and the pixel density, which says whether a screenshot
//! was taken on a 2x (Retina) screen.

use crate::ascii::{self, Image};
use crate::rotate::Orientation;
use crate::{exif, ConvError, ErrorCode, Format, Result};

/// The longest side an SVG is drawn at.
const SVG_MAX_SIDE: u32 = 4096;

/// A decoded picture, upright.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// 8-bit RGBA, not premultiplied.
    pub image: Image,
    /// Pixels per inch, when the file says.
    pub dpi: Option<f32>,
    /// The ICC colour profile the pixels are in, when the file has one.
    pub icc: Option<Vec<u8>>,
    /// A PNG that says it is sRGB, without a profile.
    pub srgb: bool,
}

impl Picture {
    fn plain(image: Image) -> Picture {
        Picture {
            image,
            dpi: None,
            icc: None,
            srgb: false,
        }
    }
}

fn unreadable(what: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("could not read the picture: {what}"),
    )
}

/// Whether conv decodes `format` itself, with no backend.
pub fn decodes_itself(format: Format) -> bool {
    matches!(
        format,
        Format::Png | Format::Jpg | Format::Gif | Format::Webp | Format::Svg
    )
}

/// Decodes a picture conv reads itself (see `decodes_itself`), upright.
/// An SVG is drawn at its own size, as a browser shows it at 1x.
pub fn decode(bytes: &[u8], format: Format) -> Result<Picture> {
    decode_scaled(bytes, format, 1)
}

/// `decode`, drawing an SVG `svg_scale` times its own size: 2 to draw it
/// as a 2x (Retina) screen shows it.
pub fn decode_scaled(bytes: &[u8], format: Format, svg_scale: u32) -> Result<Picture> {
    match format {
        Format::Png => decode_png(bytes),
        Format::Jpg => decode_jpeg(bytes),
        Format::Gif => decode_gif(bytes),
        Format::Webp => decode_webp(bytes),
        Format::Svg => decode_svg(bytes, svg_scale.max(1)),
        other => Err(unreadable(format!(
            "conv does not decode .{} itself",
            other.ext()
        ))),
    }
}

/// The picture format `bytes` are in, from their first bytes, for a picture
/// piped in with no name to go by.
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some(Format::Png);
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some(Format::Jpg);
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some(Format::Gif);
    }
    if starts(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(Format::Webp);
    }
    if starts(b"II*\0") || starts(b"MM\0*") {
        return Some(Format::Tiff);
    }
    if starts(b"BM") {
        return Some(Format::Bmp);
    }
    if starts(&[0, 0, 1, 0]) {
        return Some(Format::Ico);
    }
    if bytes.get(4..8) == Some(b"ftyp") {
        return match bytes.get(8..12)? {
            b"avif" | b"avis" => Some(Format::Avif),
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" => Some(Format::Heic),
            b"mif1" | b"msf1" => Some(Format::Heif),
            _ => None,
        };
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("<svg") || (head.starts_with("<?xml") && head.contains("<svg")) {
        return Some(Format::Svg);
    }
    None
}

fn decode_png(bytes: &[u8]) -> Result<Picture> {
    let image = ascii::decode_png(bytes)?;
    // The header again, for what the pixels alone don't say.
    let reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .map_err(unreadable)?;
    let info = reader.info();
    let dpi = info.pixel_dims.and_then(|d| match d.unit {
        png::Unit::Meter if d.xppu > 0 => Some(d.xppu as f32 * 0.0254),
        _ => None,
    });
    let icc = info.icc_profile.as_ref().map(|p| p.to_vec());
    let srgb = info.srgb.is_some();
    let orientation = info
        .exif_metadata
        .as_ref()
        .and_then(|e| exif::orientation_in_tiff(e));
    Ok(Picture {
        image: upright(image, orientation),
        dpi,
        icc,
        srgb,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Picture> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let rgba = decoder.decode().map_err(unreadable)?;
    let info = decoder.info().ok_or_else(|| unreadable("no JPEG header"))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if decoder.output_colorspace() != Some(ColorSpace::RGBA)
        || rgba.len() != (width * height * 4) as usize
    {
        return Err(unreadable("an unexpected JPEG colour layout"));
    }
    // JFIF density: 1 is dots per inch, 2 dots per centimetre.
    let dpi = match info.pixel_density {
        1 if info.x_density > 0 => Some(f32::from(info.x_density)),
        2 if info.x_density > 0 => Some(f32::from(info.x_density) * 2.54),
        _ => None,
    };
    let orientation = decoder.exif().and_then(|e| exif::orientation_in_tiff(e));
    Ok(Picture {
        image: upright(
            Image {
                width,
                height,
                rgba,
            },
            orientation,
        ),
        dpi,
        icc: decoder.icc_profile(),
        srgb: false,
    })
}

/// A GIF's first frame, where it sits on the GIF's own canvas.
fn decode_gif(bytes: &[u8]) -> Result<Picture> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(bytes).map_err(unreadable)?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    let frame = decoder
        .read_next_frame()
        .map_err(unreadable)?
        .ok_or_else(|| unreadable("the GIF has no frames"))?;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let (fw, fh) = (u32::from(frame.width), u32::from(frame.height));
    let (left, top) = (u32::from(frame.left), u32::from(frame.top));
    for y in 0..fh.min(height.saturating_sub(top)) {
        let cols = fw.min(width.saturating_sub(left)) as usize;
        let from = (y * fw) as usize * 4;
        let to = (((top + y) * width + left) as usize) * 4;
        rgba[to..to + cols * 4].copy_from_slice(&frame.buffer[from..from + cols * 4]);
    }
    Ok(Picture::plain(Image {
        width,
        height,
        rgba,
    }))
}

/// A WebP, or the first frame of an animated one.
fn decode_webp(bytes: &[u8]) -> Result<Picture> {
    let mut decoder =
        image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(unreadable)?;
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| unreadable("it is too large"))?;
    let mut buf = vec![0; size];
    decoder.read_image(&mut buf).map_err(unreadable)?;
    let (width, height) = decoder.dimensions();
    let rgba = if decoder.has_alpha() {
        buf
    } else {
        buf.as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect()
    };
    let icc = decoder.icc_profile().ok().flatten();
    let orientation = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .and_then(|e| exif::orientation_in_tiff(&e));
    Ok(Picture {
        image: upright(
            Image {
                width,
                height,
                rgba,
            },
            orientation,
        ),
        dpi: None,
        icc,
        srgb: false,
    })
}

/// An SVG drawn at `scale` times its own size, marked with the density
/// that says so (144 dpi at 2x). Text is not drawn (resvg is built without
/// fonts).
fn decode_svg(bytes: &[u8], scale: u32) -> Result<Picture> {
    let tree =
        resvg::usvg::Tree::from_data(bytes, &ascii::image::svg_options()).map_err(unreadable)?;
    let size = tree.size();
    let side = (size.width().max(size.height()) * scale as f32).round() as u32;
    let image = ascii::from_svg(bytes, side.clamp(1, SVG_MAX_SIDE))?;
    Ok(Picture {
        dpi: (scale > 1).then_some(72.0 * scale as f32),
        ..Picture::plain(image)
    })
}

/// The picture the way `orientation` says it is shown: mirrored, then
/// turned clockwise.
fn upright(image: Image, orientation: Option<Orientation>) -> Image {
    let Some(o) = orientation else {
        return image;
    };
    let mut image = if o.mirrored() { mirror(image) } else { image };
    for _ in 0..o.clockwise() / 90 {
        image = turn_right(image);
    }
    image
}

fn mirror(mut image: Image) -> Image {
    let w = image.width as usize;
    for row in image.rgba.chunks_exact_mut(w * 4) {
        for x in 0..w / 2 {
            for c in 0..4 {
                row.swap(x * 4 + c, (w - 1 - x) * 4 + c);
            }
        }
    }
    image
}

/// A quarter turn clockwise: the bottom-left pixel becomes the top-left.
fn turn_right(image: Image) -> Image {
    let (w, h) = (image.width as usize, image.height as usize);
    let mut rgba = vec![0u8; image.rgba.len()];
    for y in 0..h {
        for x in 0..w {
            // (x, y) lands at column h-1-y of row x.
            let from = (y * w + x) * 4;
            let to = (x * h + (h - 1 - y)) * 4;
            rgba[to..to + 4].copy_from_slice(&image.rgba[from..from + 4]);
        }
    }
    Image {
        width: h as u32,
        height: w as u32,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 picture whose pixels are numbered 0 to 5, row by row, in red.
    fn numbered() -> Image {
        Image {
            width: 3,
            height: 2,
            rgba: (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect(),
        }
    }

    fn reds(img: &Image) -> Vec<u8> {
        img.rgba.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
    }

    #[test]
    fn every_exif_orientation_comes_out_upright() {
        // Stored:  0 1 2
        //          3 4 5
        let cases: [(u16, (u32, u32), [u8; 6]); 8] = [
            (1, (3, 2), [0, 1, 2, 3, 4, 5]),
            (2, (3, 2), [2, 1, 0, 5, 4, 3]),
            (3, (3, 2), [5, 4, 3, 2, 1, 0]),
            (4, (3, 2), [3, 4, 5, 0, 1, 2]),
            // Transpose.
            (5, (2, 3), [0, 3, 1, 4, 2, 5]),
            // A quarter turn right: the left column becomes the top row.
            (6, (2, 3), [3, 0, 4, 1, 5, 2]),
            // Transverse.
            (7, (2, 3), [5, 2, 4, 1, 3, 0]),
            (8, (2, 3), [2, 5, 1, 4, 0, 3]),
        ];
        for (tag, size, want) in cases {
            let img = upright(numbered(), Orientation::from_exif(tag));
            assert_eq!((img.width, img.height), size, "tag {tag}");
            assert_eq!(reds(&img), want, "tag {tag}");
        }
    }

    fn png(width: u32, height: u32, rgba: &[u8], ppm: Option<u32>, exif: Option<&[u8]>) -> Vec<u8> {
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = png::BitDepth::Eight;
        info.pixel_dims = ppm.map(|p| png::PixelDimensions {
            xppu: p,
            yppu: p,
            unit: png::Unit::Meter,
        });
        info.exif_metadata = exif.map(|e| std::borrow::Cow::Owned(e.to_vec()));
        let mut out = Vec::new();
        let mut writer = png::Encoder::with_info(&mut out, info)
            .unwrap()
            .write_header()
            .unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn a_png_keeps_its_density() {
        let bytes = png(1, 1, &[1, 2, 3, 255], Some(5669), None);
        let p = decode(&bytes, Format::Png).unwrap();
        assert_eq!(p.image.rgba, [1, 2, 3, 255]);
        assert!((p.dpi.unwrap() - 144.0).abs() < 0.1, "{:?}", p.dpi);
        assert_eq!(p.icc, None);
    }

    #[test]
    fn a_png_with_an_exif_turn_comes_out_upright() {
        // II, 42, IFD at 8: one entry, Orientation SHORT 1 = 6.
        let tiff = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let rgba: Vec<u8> = (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect();
        let p = decode(&png(3, 2, &rgba, None, Some(tiff)), Format::Png).unwrap();
        assert_eq!((p.image.width, p.image.height), (2, 3));
        assert_eq!(reds(&p.image), [3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn a_gif_frame_sits_where_it_says_on_its_canvas() {
        let mut out = Vec::new();
        {
            let mut e = gif::Encoder::new(&mut out, 4, 2, &[0, 0, 0, 255, 0, 0]).unwrap();
            let mut frame = gif::Frame {
                width: 2,
                height: 1,
                left: 1,
                top: 1,
                buffer: std::borrow::Cow::Owned(vec![1, 1]),
                ..gif::Frame::default()
            };
            frame.transparent = None;
            e.write_frame(&frame).unwrap();
        }
        let p = decode(&out, Format::Gif).unwrap();
        assert_eq!((p.image.width, p.image.height), (4, 2));
        let px = |x: usize, y: usize| &p.image.rgba[(y * 4 + x) * 4..][..4];
        assert_eq!(px(1, 1), [255, 0, 0, 255]);
        assert_eq!(px(2, 1), [255, 0, 0, 255]);
        assert_eq!(px(0, 0)[3], 0, "outside the frame is clear");
    }

    #[test]
    fn an_svg_is_drawn_at_its_size_or_twice_it() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="30" height="20"><rect width="30" height="20" fill="#f00"/></svg>"##;
        let p = decode(svg, Format::Svg).unwrap();
        assert_eq!((p.image.width, p.image.height), (30, 20));
        assert_eq!(p.dpi, None);
        let p = decode_scaled(svg, Format::Svg, 2).unwrap();
        assert_eq!((p.image.width, p.image.height), (60, 40));
        assert_eq!(p.dpi, Some(144.0));
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_panic() {
        for f in [
            Format::Png,
            Format::Jpg,
            Format::Gif,
            Format::Webp,
            Format::Svg,
        ] {
            assert!(decode(b"nope", f).is_err(), "{f:?}");
        }
        assert!(decode(b"nope", Format::Heic).is_err());
    }

    #[test]
    fn formats_are_told_from_their_first_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(Format::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpg));
        assert_eq!(sniff(b"GIF89a..."), Some(Format::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        assert_eq!(sniff(b"\0\0\0\x18ftypheic\0\0"), Some(Format::Heic));
        assert_eq!(sniff(b"\0\0\0\x18ftypavif\0\0"), Some(Format::Avif));
        assert_eq!(sniff(b"  <?xml version=\"1.0\"?><svg>"), Some(Format::Svg));
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(sniff(b""), None);
    }
}
