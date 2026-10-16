//! QR codes, drawn by convkit itself (`conv qr TEXT`), with no backend.
//!
//! The code holds the text itself. Nothing sits between a scan and the
//! link, so unlike the "dynamic" codes generator sites hand out, it cannot
//! expire, and nobody counts the scans.
//!
//! Every code is drawn as one SVG: a light background with the standard
//! four-module border, the dark modules as a single path, and an optional
//! logo in the middle on a light square. That SVG is the `.svg` output, and
//! resvg draws the PNG from it, so the two can never disagree. Before
//! anything is written, the drawing is read back with a QR decoder and has
//! to give back exactly the text it was made from.

use std::fmt::Write as _;
use std::path::Path;

use base64::Engine as _;
use qrcode::bits::Bits;
use qrcode::types::QrError;
use qrcode::{Color, EcLevel, QrCode, Version};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

use crate::{ConvError, ErrorCode, Format, Result};

/// The light border, in modules, that the standard asks for on every side.
const QUIET_ZONE: usize = 4;

/// The PNG's width and height when `--size` is not given.
pub const DEFAULT_SIZE: u32 = 1024;

/// The largest `--size`. A drawing this big already takes 400 MB to draw.
pub const MAX_SIZE: u32 = 10_000;

/// The logo's light square is about this fraction of the code's width. At
/// the high error correction a logo brings, a code can lose 30% of itself
/// and still read; a fifth of the width covers 4-6% of it.
const LOGO_FRACTION: f64 = 0.2;

/// The decoder reads a drawing with at most this many pixels a module. Its
/// shapes are the same at any whole scale, so a bigger output is checked
/// at this one rather than spending a second on every million pixels.
const CHECK_SCALE: u32 = 8;

/// A logo to put in the middle of the code.
pub struct Logo {
    bytes: Vec<u8>,
    mime: &'static str,
    /// Whether an SVG logo has text, which the PNG cannot draw (resvg is
    /// built without fonts) but a browser showing the SVG can.
    has_text: bool,
    name: String,
}

impl Logo {
    /// Reads a logo: SVG, PNG, JPG, GIF or WebP.
    pub fn read(path: &Path) -> Result<Logo> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "--logo takes an svg, png, jpg, gif or webp file, not {}",
                        path.display()
                    ),
                ))
            }
        };
        let bytes = std::fs::read(path).map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                ErrorCode::InputNotFound
            } else {
                ErrorCode::InvalidInvocation
            };
            ConvError::new(code, format!("cannot read {}: {e}", path.display()))
        })?;
        let has_text = mime == "image/svg+xml" && String::from_utf8_lossy(&bytes).contains("<text");
        Ok(Logo {
            bytes,
            mime,
            has_text,
            name: path.display().to_string(),
        })
    }
}

/// A drawn and checked QR code.
pub struct Qr {
    /// The code as a self-contained SVG, logo included.
    pub svg: String,
    /// The SVG's width and height in pixels: `--size`, rounded down to a
    /// whole number of pixels a module so every edge is sharp.
    pub size: u32,
    /// The QR version, 1 (21 modules across) to 40 (177).
    pub version: i16,
    /// "M" (15% of the code can be lost and it still reads) or, with a
    /// logo, "H" (30%).
    pub error_correction: &'static str,
    logo_has_text: bool,
    tree: usvg::Tree,
}

impl Qr {
    /// The code as a PNG, `size` pixels square.
    pub fn png(&self) -> Result<Vec<u8>> {
        let pixmap = draw(&self.tree, self.size)?;
        encode_png(&pixmap, self.has_colour())
    }

    /// What the result leaves out, for a file written as `format`.
    pub fn warnings(&self, format: Format) -> Vec<String> {
        let mut w = Vec::new();
        if self.logo_has_text && format != Format::Svg {
            w.push(
                "the logo's text is not drawn; convert its text to outlines (paths) to keep it"
                    .to_string(),
            );
        }
        w
    }

    fn has_colour(&self) -> bool {
        self.svg.contains("<image")
    }
}

/// Encodes `text` and draws it about `size` pixels square, with `logo` in
/// the middle, then reads the drawing back to prove it scans.
pub fn make(text: &str, logo: Option<&Logo>, size: u32) -> Result<Qr> {
    if text.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "there is no text to put in the code",
        ));
    }
    let ec = if logo.is_some() {
        EcLevel::H
    } else {
        EcLevel::M
    };
    let code = encode(text, ec, logo.is_some())?;
    let n = code.width();
    let across = n + 2 * QUIET_ZONE;
    let scale = size / across as u32;
    if scale == 0 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("--size {size} is too small for this code, which is {across} modules across; give at least {across}"),
        ));
    }
    let px = scale * across as u32;
    let svg = svg_for(&code, logo, px);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not draw the code: {e}"),
        )
    })?;
    if let Some(logo) = logo {
        if !has_image(tree.root()) {
            return Err(ConvError::new(
                ErrorCode::ConversionFailed,
                format!("could not read the logo {}", logo.name),
            ));
        }
    }
    check(&tree, px, scale, text)?;
    Ok(Qr {
        svg,
        size: px,
        version: match code.version() {
            Version::Normal(v) | Version::Micro(v) => v,
        },
        error_correction: if logo.is_some() { "H" } else { "M" },
        logo_has_text: logo.is_some_and(|l| l.has_text),
        tree,
    })
}

/// The smallest code that holds `text`. ASCII goes through the encoder's
/// own choice of modes (digits and capitals pack tighter). Anything else is
/// kept as UTF-8 bytes, since that choice would read some UTF-8 pairs as
/// Shift JIS kanji, and a scanner would then show different characters.
fn encode(text: &str, ec: EcLevel, with_logo: bool) -> Result<QrCode> {
    let encoded = if text.is_ascii() {
        QrCode::with_error_correction_level(text, ec)
    } else {
        (1..=40)
            .find_map(|v| {
                let mut bits = Bits::new(Version::Normal(v));
                bits.push_byte_data(text.as_bytes()).ok()?;
                bits.push_terminator(ec).ok()?;
                Some(bits)
            })
            .ok_or(QrError::DataTooLong)
            .and_then(|bits| QrCode::with_bits(bits, ec))
    };
    encoded.map_err(|e| match e {
        QrError::DataTooLong => {
            let unit = if text.is_ascii() {
                "characters"
            } else {
                "bytes"
            };
            let limit = if with_logo {
                "1,273, or 2,331 without --logo"
            } else {
                "2,331"
            };
            ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "too much text for one QR code: {} {unit}, and a code holds at most {limit}",
                    text.len()
                ),
            )
        }
        other => ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not encode the text: {other}"),
        ),
    })
}

/// The code as SVG, `px` pixels square, in a coordinate space of one unit
/// a module. Each run of dark modules in a row is one rectangle of a single
/// path, so the file stays small and nothing shows a seam.
fn svg_for(code: &QrCode, logo: Option<&Logo>, px: u32) -> String {
    let n = code.width();
    let across = n + 2 * QUIET_ZONE;
    let colors = code.to_colors();
    let mut d = String::new();
    for y in 0..n {
        let row = &colors[y * n..(y + 1) * n];
        let mut x = 0;
        while x < n {
            if row[x] == Color::Light {
                x += 1;
                continue;
            }
            let start = x;
            while x < n && row[x] == Color::Dark {
                x += 1;
            }
            let len = x - start;
            let _ = write!(
                d,
                "M{} {}h{len}v1h-{len}z",
                start + QUIET_ZONE,
                y + QUIET_ZONE
            );
        }
    }
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{px}\" height=\"{px}\" viewBox=\"0 0 {across} {across}\">\n\
         <rect width=\"{across}\" height=\"{across}\" fill=\"#fff\"/>\n\
         <path fill=\"#000\" shape-rendering=\"crispEdges\" d=\"{d}\"/>\n"
    );
    if let Some(logo) = logo {
        // An odd side centres on the odd-sized code, on whole modules.
        let mut side = (n as f64 * LOGO_FRACTION).ceil() as usize;
        if side % 2 == 0 {
            side += 1;
        }
        let at = QUIET_ZONE + (n - side) / 2;
        let data = base64::engine::general_purpose::STANDARD.encode(&logo.bytes);
        let _ = write!(
            svg,
            "<rect x=\"{at}\" y=\"{at}\" width=\"{side}\" height=\"{side}\" fill=\"#fff\" shape-rendering=\"crispEdges\"/>\n\
             <image x=\"{inner_at}\" y=\"{inner_at}\" width=\"{inner}\" height=\"{inner}\" \
             preserveAspectRatio=\"xMidYMid meet\" xlink:href=\"data:{mime};base64,{data}\"/>\n",
            inner_at = at as f64 + 0.5,
            inner = side - 1,
            mime = logo.mime,
        );
    }
    svg.push_str("</svg>\n");
    svg
}

/// Whether the drawing kept an image. usvg drops one it cannot decode, which
/// would leave an empty square where the logo should be.
fn has_image(group: &usvg::Group) -> bool {
    group.children().iter().any(|node| match node {
        usvg::Node::Image(_) => true,
        usvg::Node::Group(g) => has_image(g),
        _ => false,
    })
}

/// Draws the tree `px` pixels square.
fn draw(tree: &usvg::Tree, px: u32) -> Result<Pixmap> {
    let mut pixmap = Pixmap::new(px, px).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot draw a {px}x{px} picture"),
        )
    })?;
    let s = px as f32 / tree.size().width();
    resvg::render(tree, Transform::from_scale(s, s), &mut pixmap.as_mut());
    Ok(pixmap)
}

/// Reads the drawing back with a QR decoder and refuses a code that does
/// not give back `text` exactly.
fn check(tree: &usvg::Tree, px: u32, scale: u32, text: &str) -> Result<()> {
    let pixmap = draw(tree, px / scale * scale.min(CHECK_SCALE))?;
    let w = pixmap.width() as usize;
    let data = pixmap.data();
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, w, |x, y| {
        let i = (y * w + x) * 4;
        let (r, g, b) = (data[i] as u32, data[i + 1] as u32, data[i + 2] as u32);
        ((r * 299 + g * 587 + b * 114) / 1000) as u8
    });
    let read = img
        .detect_grids()
        .into_iter()
        .any(|grid| grid.decode().is_ok_and(|(_, content)| content == text));
    if read {
        Ok(())
    } else {
        let hint = if scale < 3 {
            "; try a larger --size"
        } else {
            ""
        };
        Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("the drawn code did not read back as the text, so nothing was written{hint}"),
        ))
    }
}

/// A PNG of the opaque drawing: greyscale for black and white, RGB when a
/// logo brings colour. Neither needs the alpha channel the pixmap carries.
fn encode_png(pixmap: &Pixmap, colour: bool) -> Result<Vec<u8>> {
    let (colour_type, pixels): (png::ColorType, Vec<u8>) = if colour {
        let rgb = pixmap
            .data()
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        (png::ColorType::Rgb, rgb)
    } else {
        let grey = pixmap.data().chunks_exact(4).map(|p| p[0]).collect();
        (png::ColorType::Grayscale, grey)
    };
    let failed = |e: png::EncodingError| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not write the PNG: {e}"),
        )
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, pixmap.width(), pixmap.height());
    encoder.set_color(colour_type);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(&pixels).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(png: &[u8]) -> Vec<String> {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let (w, h) = (info.width as usize, info.height as usize);
        let channels = info.color_type.samples();
        let mut img =
            rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| buf[(y * w + x) * channels]);
        img.detect_grids()
            .into_iter()
            .filter_map(|g| g.decode().ok().map(|(_, s)| s))
            .collect()
    }

    #[test]
    fn a_link_reads_back_from_the_png() {
        let qr = make("https://example.com/menu?table=4", None, DEFAULT_SIZE).unwrap();
        assert_eq!(qr.error_correction, "M");
        assert_eq!(
            decoded(&qr.png().unwrap()),
            ["https://example.com/menu?table=4"]
        );
    }

    #[test]
    fn the_size_snaps_to_whole_pixels_a_module() {
        // "hi" fits version 1: 21 modules plus a 4-module border each side.
        let qr = make("hi", None, 1000).unwrap();
        assert_eq!(qr.version, 1);
        assert_eq!(qr.size, 29 * (1000 / 29));
        assert!(qr.svg.contains(&format!("width=\"{}\"", qr.size)));
    }

    #[test]
    fn a_size_smaller_than_the_code_is_refused() {
        let e = make("hi", None, 20).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("at least 29"), "{}", e.message);
    }

    #[test]
    fn non_ascii_text_reads_back_unchanged() {
        // "日本" is E6 97 A5 E6 9C AC in UTF-8, which the encoder's own mode
        // choice would take for Shift JIS kanji.
        let text = "日本語のメニュー: https://example.jp/";
        let qr = make(text, None, 600).unwrap();
        assert_eq!(decoded(&qr.png().unwrap()), [text]);
    }

    #[test]
    fn empty_text_is_refused() {
        let e = make("", None, DEFAULT_SIZE).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }

    #[test]
    fn too_much_text_names_the_limit() {
        let e = make(&"x".repeat(3000), None, DEFAULT_SIZE).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("3000 characters"), "{}", e.message);
        assert!(e.message.contains("2,331"), "{}", e.message);
    }

    #[test]
    fn the_most_text_one_code_holds_still_reads_back() {
        let text = "x".repeat(2331);
        let qr = make(&text, None, DEFAULT_SIZE).unwrap();
        assert_eq!(qr.version, 40);
        assert_eq!(decoded(&qr.png().unwrap()), [text]);
    }

    fn svg_logo(body: &str) -> Logo {
        Logo {
            bytes: format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\">{body}</svg>"
            )
            .into_bytes(),
            mime: "image/svg+xml",
            has_text: body.contains("<text"),
            name: "logo.svg".into(),
        }
    }

    #[test]
    fn a_logo_raises_error_correction_and_still_reads_back() {
        let logo = svg_logo("<circle cx=\"5\" cy=\"5\" r=\"5\" fill=\"#e33\"/>");
        let qr = make("https://example.com", Some(&logo), DEFAULT_SIZE).unwrap();
        assert_eq!(qr.error_correction, "H");
        assert!(qr.svg.contains("data:image/svg+xml;base64,"));
        assert_eq!(decoded(&qr.png().unwrap()), ["https://example.com"]);
    }

    #[test]
    fn a_logo_that_does_not_decode_is_refused() {
        let logo = Logo {
            bytes: b"not a png".to_vec(),
            mime: "image/png",
            has_text: false,
            name: "logo.png".into(),
        };
        let e = make("https://example.com", Some(&logo), DEFAULT_SIZE)
            .err()
            .unwrap();
        assert!(
            e.message.contains("could not read the logo logo.png"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_logo_with_text_warns_for_pictures_only() {
        let logo = svg_logo("<text x=\"0\" y=\"9\">Hi</text><rect width=\"10\" height=\"2\"/>");
        let qr = make("https://example.com", Some(&logo), DEFAULT_SIZE).unwrap();
        assert!(qr.warnings(Format::Svg).is_empty());
        assert_eq!(qr.warnings(Format::Png).len(), 1);
    }

    #[test]
    fn a_logo_of_another_kind_is_refused() {
        let e = Logo::read(Path::new("logo.bmp")).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }
}
