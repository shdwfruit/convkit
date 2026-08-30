//! Exact unique-RGB24-colour counting for GIF outputs -- the axis that
//! specifically gates the future imagequant swap for ffmpeg's
//! `palettegen`/`paletteuse` (see `registry::GIF_FILTER` in convkit-core),
//! where the whole point of the swap is a *better* palette, not merely an
//! equal one.
//!
//! The counting method (a 2^24-bit exact bitset over every possible RGB24
//! colour, indexed by `(r<<16)|(g<<8)|b`) is the same one
//! `crates/convkit-core/examples/count_colors.rs` already implements and
//! documents as the reference technique -- ffmpeg-decoded raw RGB24, exact
//! popcount, no sampling or hashing. It's reimplemented here rather than
//! imported because that file is a standalone binary example (`cargo run
//! --example count_colors`), not a library function convkit-core exports,
//! and this task's constraint is to leave convkit-core's own source
//! untouched -- adding a new `pub fn` to it, even a purely additive one,
//! would be exactly that kind of edit. The algorithm itself is unchanged
//! from that reference; only the input plumbing differs (piped ffmpeg
//! stdout here, a file argument there).

use std::path::Path;
use std::process::{Command, Stdio};

/// Runs `ffmpeg -i <file> -f rawvideo -pix_fmt rgb24 -` and counts the
/// exact number of distinct RGB24 colours across every frame in the
/// decoded output, via the bitset method count_colors.rs documents.
/// Reads ffmpeg's stdout directly (piped, not a temp file) since a GIF
/// fixture in this harness's corpus is always small enough to hold the raw
/// dump in memory.
pub fn unique_colors(ffmpeg: &Path, file: &Path) -> Option<u64> {
    let out = Command::new(ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(file)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(count_unique_rgb24(&out.stdout))
}

/// The reference algorithm from `count_colors.rs`: one bit per possible
/// 24-bit RGB colour, set exactly once per distinct colour encountered, no
/// collisions possible since the index space *is* the colour space.
fn count_unique_rgb24(data: &[u8]) -> u64 {
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
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_distinct_colours_exactly_once_each() {
        // Three pixels: red, green, red again -- 2 distinct colours.
        let data = [255u8, 0, 0, 0, 255, 0, 255, 0, 0];
        assert_eq!(count_unique_rgb24(&data), 2);
    }

    #[test]
    fn empty_input_has_zero_colours() {
        assert_eq!(count_unique_rgb24(&[]), 0);
    }

    #[test]
    fn every_pixel_distinct_counts_every_one() {
        let mut data = Vec::new();
        for r in 0..4u8 {
            for g in 0..4u8 {
                data.extend_from_slice(&[r, g, 0]);
            }
        }
        assert_eq!(count_unique_rgb24(&data), 16);
    }
}
