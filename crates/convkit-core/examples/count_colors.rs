//! Counts distinct RGB24 triplets in a raw `rgb24` frame dump.
//!
//! Built for Task 15's GIF palette calibration
//! (`docs/defaults-calibration.md` §2, "GIF via generated palette"): with
//! no ImageMagick (`magick identify -format %k`) available on the machine
//! that calibration was measured on, this is the "ffmpeg-based colour
//! analysis" the task brief allows in its place. It is a general-purpose
//! measurement tool, not a test -- there is nothing here to assert against,
//! since the expected colour count depends entirely on the input.
//!
//! Not wired into the crate's public API on purpose: it exists to be run
//! by hand, once, to produce a number for a doc, and to be re-run by
//! anyone auditing that number later. `cargo run -p convkit-core --example
//! count_colors` builds and runs it without adding a dependency or an
//! API surface to the library itself.
//!
//! # Producing the input
//!
//! ffmpeg can decode any image or video ffmpeg reads (including a GIF) to
//! a raw, uncompressed RGB24 stream:
//!
//! ```text
//! ffmpeg -i input.gif -f rawvideo -pix_fmt rgb24 frames.raw
//! ```
//!
//! That dump is a flat sequence of 3-byte (R, G, B) pixels, frame after
//! frame with no header, footer, or per-frame separator -- which is why
//! this tool doesn't need to know width, height, or frame count: every
//! 3-byte chunk is one pixel, in any order, from however many frames were
//! in the input.
//!
//! # Usage
//!
//! ```text
//! cargo run -p convkit-core --example count_colors -- frames.raw
//! ```
//!
//! # Method
//!
//! One bit per possible 24-bit RGB colour: a 2^24-bit (2 MiB) bitset
//! indexed by `(r << 16) | (g << 8) | b`. Exact, not sampled or
//! hash-approximated -- every distinct colour in the input sets exactly
//! one bit, so the final popcount is the true distinct-colour count, with
//! no collision risk (the index space is the colour space itself, not a
//! hash of it).

use std::env;
use std::fs;

fn main() {
    let path = env::args()
        .nth(1)
        .expect("usage: count_colors <raw-rgb24-file>");
    let data = fs::read(&path).expect("read raw file");
    assert_eq!(
        data.len() % 3,
        0,
        "file length must be a multiple of 3 (rgb24 is 3 bytes/pixel); \
         got {} bytes -- was this really produced with -pix_fmt rgb24?",
        data.len()
    );

    // 2^24 bits = 2 MiB, one bit per possible colour.
    let mut seen = vec![0u64; (1usize << 24) / 64];
    let mut unique: u64 = 0;
    for px in data.chunks_exact(3) {
        let idx = ((px[0] as usize) << 16) | ((px[1] as usize) << 8) | (px[2] as usize);
        let word = idx / 64;
        let bit = 1u64 << (idx % 64);
        if seen[word] & bit == 0 {
            seen[word] |= bit;
            unique += 1;
        }
    }
    let pixel_count = data.len() / 3;
    println!("{path}: {pixel_count} pixels, {unique} unique RGB24 colours");
}
