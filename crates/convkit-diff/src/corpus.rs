//! Corpus handling: walking an arbitrary directory of files (synthetic or
//! user-supplied -- "the owner may point this at a real photo library
//! later"), and `gen-corpus`'s synthesis of the adversarial fixtures named
//! in the harness brief: every EXIF orientation, a non-sRGB ICC profile,
//! progressive/CMYK JPEG, palette/16-bit/alpha PNG, palette/grayscale
//! TIFF, a transparent SVG, and a multi-frame GIF -- plus the two real
//! fixtures already in the repo (`photo.heic`, `clip.mp4`).

use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::{Backend, Resolver};

use crate::synth;

/// Every regular file under `root`, recursed, in a deterministic
/// (sorted-by-path) order. Skips dotfiles/dot-directories (`.git`,
/// `.convkit-diff.refs`, and the like -- an ordinary hygiene rule for
/// walking a real, arbitrary directory, not specific to this corpus) and
/// silently skips a directory entry it cannot read rather than aborting the
/// whole walk, since a real photo library can easily contain a permission-
/// denied subfolder.
pub fn walk_corpus(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_into(root, &mut out);
    out.sort();
    out
}

fn walk_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dotfile = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
        if is_dotfile {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_into(&path, out);
        } else if file_type.is_file() {
            out.push(path);
        }
    }
}

/// A corpus file's path relative to the corpus root, forward-slash
/// normalised so baseline keys are identical across platforms.
pub fn relative_slash(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub struct GenReport {
    pub written: Vec<PathBuf>,
    pub notes: Vec<String>,
}

fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run {what}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what} failed: {}", stderr.trim()));
    }
    Ok(())
}

/// Locates `tests/fixtures/<name>` relative to the workspace this crate was
/// built from. Best-effort: `gen-corpus` still produces every synthetic
/// fixture even when run from a copy of the binary that's lost track of the
/// source tree (e.g. installed elsewhere) -- the real fixtures are simply
/// skipped, with a note, rather than aborting the whole run.
fn workspace_fixture(name: &str) -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join("../../tests/fixtures").join(name);
    candidate.canonicalize().ok()
}

/// Generates the full adversarial + real-fixture corpus described in the
/// harness brief into `out_dir` (created if missing). Returns every file
/// written, plus human-readable notes about anything skipped (a real
/// fixture that couldn't be located) -- never a hard error for that case,
/// since a corpus missing one nice-to-have fixture is still a working
/// corpus; a `magick`/`ffmpeg` invocation failing outright, on the other
/// hand, *is* a hard error, since it means the generator itself is broken
/// on this machine.
pub fn gen_corpus(out_dir: &Path, resolver: &Resolver) -> Result<GenReport, String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;

    let magick = resolver
        .resolve(Backend::Magick)
        .map_err(|e| format!("cannot resolve ImageMagick: {}", e.message))?
        .path;

    let mut written = Vec::new();
    let mut notes = Vec::new();
    let mut w = |p: PathBuf| {
        written.push(p);
    };

    // --- A colored, asymmetric base image. Colored (not grayscale) so the
    // colorspace axis defaults to sRGB like a real photo, and asymmetric
    // (an off-center rectangle) so a 90-degree auto-orient rotation is both
    // visually and dimensionally detectable, not a no-op on a symmetric
    // canvas. Not itself part of the corpus -- an intermediate the rest of
    // this function builds from.
    let base = out_dir.join(".base.png");
    run(
        Command::new(&magick)
            .args(["-size", "64x48", "xc:#3355ee", "-fill", "#ffaa00", "-draw"])
            .arg("rectangle 4,4 24,20")
            .arg(&base),
        "generate base image",
    )?;

    // --- All eight EXIF orientation values. -----------------------------
    let plain_jpeg = out_dir.join(".plain.jpg");
    run(
        Command::new(&magick).arg(&base).arg(&plain_jpeg),
        "encode plain jpeg for orientation fixtures",
    )?;
    let jpeg_bytes = std::fs::read(&plain_jpeg)
        .map_err(|e| format!("cannot read {}: {e}", plain_jpeg.display()))?;
    for orientation in 1u16..=8 {
        let bytes = synth::inject_exif_orientation(&jpeg_bytes, orientation)?;
        let path = out_dir.join(format!("exif-orientation-{orientation}.jpg"));
        std::fs::write(&path, bytes)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        w(path);
    }
    let _ = std::fs::remove_file(&plain_jpeg);

    // --- Embedded ICC profile that is not sRGB. --------------------------
    let icc_path = out_dir.join(".synthetic.icc");
    std::fs::write(
        &icc_path,
        synth::synthetic_non_srgb_icc("convkit-diff non-sRGB test profile"),
    )
    .map_err(|e| format!("cannot write {}: {e}", icc_path.display()))?;
    let icc_fixture = out_dir.join("icc-nonsrgb.jpg");
    run(
        Command::new(&magick)
            .arg(&base)
            .arg("-profile")
            .arg(&icc_path)
            .arg(&icc_fixture),
        "embed non-sRGB ICC profile",
    )?;
    w(icc_fixture);
    let _ = std::fs::remove_file(&icc_path);

    // --- Progressive JPEG. ------------------------------------------------
    let progressive = out_dir.join("jpeg-progressive.jpg");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-interlace", "JPEG"])
            .arg(&progressive),
        "encode progressive jpeg",
    )?;
    w(progressive);

    // --- CMYK JPEG. ---------------------------------------------------------
    let cmyk = out_dir.join("jpeg-cmyk.jpg");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-colorspace", "CMYK"])
            .arg(&cmyk),
        "encode CMYK jpeg",
    )?;
    w(cmyk);

    // --- Palette PNG. ------------------------------------------------------
    let palette_png = out_dir.join("png-palette.png");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-colors", "64"])
            .arg(format!("PNG8:{}", palette_png.display())),
        "encode palette PNG",
    )?;
    w(palette_png);

    // --- 16-bit PNG. --------------------------------------------------------
    let png16 = out_dir.join("png-16bit.png");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-depth", "16"])
            .arg(&png16),
        "encode 16-bit PNG",
    )?;
    w(png16);

    // --- PNG with alpha. -----------------------------------------------------
    let alpha_png = out_dir.join("png-alpha.png");
    run(
        Command::new(&magick)
            .args(["-size", "64x48", "xc:none", "-fill", "#22bb6699", "-draw"])
            .arg("circle 32,24 32,8")
            .arg(&alpha_png),
        "encode PNG with alpha",
    )?;
    w(alpha_png);

    // --- Palette TIFF and grayscale TIFF (predicted to fail outright under
    // a naive `image`-crate port). --------------------------------------
    let palette_tiff = out_dir.join("tiff-palette.tiff");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-colors", "64"])
            .arg(&palette_tiff),
        "encode palette TIFF",
    )?;
    w(palette_tiff);

    let gray_tiff = out_dir.join("tiff-grayscale.tiff");
    run(
        Command::new(&magick)
            .arg(&base)
            .args(["-colorspace", "Gray"])
            .arg(&gray_tiff),
        "encode grayscale TIFF",
    )?;
    w(gray_tiff);

    // --- SVG with transparency (the svg->jpg black-background bug's own
    // regression fixture). ------------------------------------------------
    let svg_path = out_dir.join("svg-transparent.svg");
    std::fs::write(
        &svg_path,
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
  <circle cx="32" cy="32" r="24" fill="red" fill-opacity="0.6"/>
</svg>
"#,
    )
    .map_err(|e| format!("cannot write {}: {e}", svg_path.display()))?;
    w(svg_path);

    // --- Multi-frame GIF. -----------------------------------------------
    let gif_path = out_dir.join("gif-multiframe.gif");
    run(
        Command::new(&magick)
            .args(["-size", "64x48", "-delay", "20", "-loop", "0"])
            .args(["xc:red", "xc:#22bb66", "xc:#3355ee", "xc:yellow"])
            .arg(&gif_path),
        "encode multi-frame GIF",
    )?;
    w(gif_path);

    // --- A plain, non-adversarial baseline case. -------------------------
    let gradient = out_dir.join("plain-gradient.png");
    run(
        Command::new(&magick)
            .args(["-size", "96x64", "gradient:#ff8800-#0066ff"])
            .arg(&gradient),
        "encode plain gradient PNG",
    )?;
    w(gradient);

    let _ = std::fs::remove_file(&base);

    // --- Real fixtures already in the repo. -------------------------------
    for name in ["photo.heic", "clip.mp4"] {
        match workspace_fixture(name) {
            Some(src) => {
                let dest = out_dir.join(name);
                std::fs::copy(&src, &dest).map_err(|e| {
                    format!("cannot copy {} to {}: {e}", src.display(), dest.display())
                })?;
                w(dest);
            }
            None => notes.push(format!(
                "skipped real fixture {name}: could not locate tests/fixtures/{name} \
                 relative to this build (CARGO_MANIFEST_DIR={})",
                env!("CARGO_MANIFEST_DIR")
            )),
        }
    }

    written.sort();
    Ok(GenReport { written, notes })
}
