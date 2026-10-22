//! The picture `conv ascii` draws from, as 8-bit RGBA.

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

use crate::{ConvError, ErrorCode, Result};

/// A picture as 8-bit RGBA, row by row, not premultiplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn unreadable(what: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("could not read the picture: {what}"),
    )
}

/// Decodes a PNG of any colour type and bit depth into 8-bit RGBA.
pub fn decode_png(bytes: &[u8]) -> Result<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(unreadable)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| unreadable("it is too large"))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(unreadable)?;
    buf.truncate(info.buffer_size());
    let (colour, _) = reader.output_color_type();
    let rgba = match colour {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        // EXPAND turns a palette into RGB or RGBA.
        png::ColorType::Indexed => return Err(unreadable("an unexpanded palette")),
    };
    Ok(Image {
        width: info.width,
        height: info.height,
        rgba,
    })
}

/// Draws an SVG so its longer side is `max_side` pixels, keeping its
/// shape. Text is not drawn (resvg is built without fonts); see
/// `svg_has_text`.
pub fn from_svg(bytes: &[u8], max_side: u32) -> Result<Image> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).map_err(unreadable)?;
    let size = tree.size();
    let scale = max_side as f32 / size.width().max(size.height());
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap = Pixmap::new(width, height)
        .ok_or_else(|| unreadable(format!("cannot draw it at {width}x{height}")))?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia keeps premultiplied colour; undo it.
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Whether an SVG has text, which `from_svg` cannot draw.
pub fn svg_has_text(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).contains("<text")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(
        colour: png::ColorType,
        depth: png::BitDepth,
        w: u32,
        h: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(colour);
        e.set_depth(depth);
        let mut writer = e.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn decode_png_gives_rgba_for_every_colour_type() {
        use png::{BitDepth::*, ColorType::*};
        let cases: Vec<(Vec<u8>, [u8; 4])> = vec![
            (
                encode(Rgba, Eight, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]),
                [1, 2, 3, 4],
            ),
            (
                encode(Rgb, Eight, 2, 1, &[1, 2, 3, 4, 5, 6]),
                [1, 2, 3, 255],
            ),
            (
                encode(Grayscale, Eight, 2, 1, &[200, 100]),
                [200, 200, 200, 255],
            ),
            (
                encode(GrayscaleAlpha, Eight, 2, 1, &[200, 7, 100, 9]),
                [200, 200, 200, 7],
            ),
            (
                encode(Rgb, Sixteen, 2, 1, &[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]),
                [1, 2, 3, 255],
            ),
        ];
        for (bytes, first) in cases {
            let img = decode_png(&bytes).unwrap();
            assert_eq!((img.width, img.height, img.rgba.len()), (2, 1, 8));
            assert_eq!(img.rgba[..4], first);
        }
    }

    #[test]
    fn a_broken_png_is_an_error() {
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn an_svg_keeps_its_shape() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100"><rect width="100" height="100" fill="#fff"/></svg>"##;
        let img = from_svg(svg, 400).unwrap();
        assert_eq!((img.width, img.height), (400, 200));
        assert_eq!(img.rgba[..4], [255, 255, 255, 255], "the rect is drawn");
        let right = ((100 * 400 + 399) * 4) as usize;
        assert_eq!(img.rgba[right + 3], 0, "outside the rect is transparent");
    }

    #[test]
    fn svg_text_is_noticed() {
        assert!(svg_has_text(b"<svg><text>hi</text></svg>"));
        assert!(!svg_has_text(b"<svg><path d=\"M0 0\"/></svg>"));
    }
}
