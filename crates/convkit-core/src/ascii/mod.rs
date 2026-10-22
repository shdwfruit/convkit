//! ASCII art from pictures (`conv ascii`): the picture read as RGBA,
//! sampled onto a grid of character cells, and drawn as text, a web page
//! or a PNG. Nothing here prints; the `conv` binary writes the results.

pub mod image;

pub use image::{decode_png, from_svg, svg_has_text, Image};

/// The longest side a picture is read at. Enough samples for 400 columns
/// of braille (800 dots); more would only slow the read.
pub const READ_MAX_SIDE: u32 = 1600;
