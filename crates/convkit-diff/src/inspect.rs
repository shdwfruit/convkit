//! ImageMagick-backed inspection of a conversion's output: dimensions,
//! colourspace, orientation, and embedded ICC profile. ImageMagick is the
//! oracle throughout (per the brief), never the `image` crate or convkit's
//! own backends -- this has to stay independent of whatever engine convkit
//! is using in order to validly judge it.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::baseline::IccInfo;

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub dimensions: Option<(u32, u32)>,
    pub colorspace: Option<String>,
    pub orientation: Option<String>,
    pub icc: Option<IccInfo>,
}

/// `identify -format "%w %h|%[colorspace]|%[orientation]" <file>[0]`,
/// parsed. `[0]` pins every token to the first frame/page, so a multi-frame
/// GIF or a multi-page TIFF still yields exactly one, well-defined
/// snapshot rather than one line per frame.
pub fn snapshot(magick: &Path, file: &Path) -> Snapshot {
    let arg = format!("{}[0]", file.display());
    let out = Command::new(magick)
        .args([
            "identify",
            "-format",
            "%w %h|%[colorspace]|%[orientation]",
            &arg,
        ])
        .output();
    let Ok(out) = out else {
        return Snapshot::default();
    };
    if !out.status.success() {
        return Snapshot::default();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().splitn(3, '|');
    let dims = parts.next().and_then(parse_dims);
    let colorspace = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let orientation = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Snapshot {
        dimensions: dims,
        colorspace,
        orientation,
        icc: extract_icc(magick, file),
    }
}

fn parse_dims(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split_whitespace();
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    Some((w, h))
}

/// Extracts the embedded ICC profile via `magick <file> icc:-`, which
/// writes the profile's raw bytes to stdout and exits non-zero with "no
/// color profile is available" when none is embedded -- confirmed against
/// this ImageMagick build directly, and the cleanest way to get both
/// presence and exact bytes (for length + hash) in one call, with no
/// dependency on any EXIF/ICC-parsing crate of our own.
pub fn extract_icc(magick: &Path, file: &Path) -> Option<IccInfo> {
    let arg = format!("{}[0]", file.display());
    let out = Command::new(magick).args([&arg, "icc:-"]).output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(&out.stdout);
    Some(IccInfo {
        len_bytes: out.stdout.len() as u64,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// One line per frame/page from `magick identify <file>` (no `-format`),
/// which is the frame count for an animated/multi-page image. Used for the
/// GIF `frame_count` axis; not called for single-frame formats.
pub fn frame_count(magick: &Path, file: &Path) -> Option<u32> {
    let out = Command::new(magick)
        .arg("identify")
        .arg(file)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let n = text.lines().filter(|l| !l.trim().is_empty()).count();
    if n == 0 {
        None
    } else {
        Some(n as u32)
    }
}

/// `magick compare -metric RMSE <a>[0] <b>[0] null:`, returning the
/// normalised (0..1) metric ImageMagick prints in parentheses on stderr,
/// e.g. `"1234.5 (0.0188411)"` -> `0.0188411`.
///
/// `[0]` pins both sides to their first frame: `compare` on a genuinely
/// multi-frame GIF was verified (by hand, against this ImageMagick build)
/// to *not* do a sensible per-frame comparison -- even two byte-identical
/// multi-frame GIFs came back with a large nonzero RMSE -- while comparing
/// frame 0 of each is well-defined and, for two identical inputs, reports
/// exactly `0`. `compare`'s exit code is 1 whenever the metric is nonzero
/// (not just on a real error), so the metric string is parsed regardless of
/// exit status; only a missing/unparsable metric is treated as failure.
pub fn compare_rmse(magick: &Path, a: &Path, b: &Path) -> Result<f64, String> {
    let arg_a = format!("{}[0]", a.display());
    let arg_b = format!("{}[0]", b.display());
    let out = Command::new(magick)
        .args(["compare", "-metric", "RMSE", &arg_a, &arg_b, "null:"])
        .output()
        .map_err(|e| format!("failed to run magick compare: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_normalised_metric(&stderr)
        .ok_or_else(|| format!("could not parse RMSE metric from: {}", stderr.trim()))
}

fn parse_normalised_metric(s: &str) -> Option<f64> {
    let open = s.find('(')?;
    let close = s[open..].find(')')? + open;
    s[open + 1..close].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_normalised_metric_out_of_a_real_compare_line() {
        assert_eq!(
            parse_normalised_metric("31046.7 (0.473743)"),
            Some(0.473743)
        );
        assert_eq!(parse_normalised_metric("0 (0)"), Some(0.0));
    }

    #[test]
    fn parses_dims() {
        assert_eq!(parse_dims("64 48"), Some((64, 48)));
        assert_eq!(parse_dims("garbage"), None);
    }
}
