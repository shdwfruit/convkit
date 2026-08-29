//! Task 15: property tests against real backend output.
//!
//! Every test here is `#[ignore]`-gated: `cargo test` stays green on a
//! machine with zero backends installed, and `cargo test -- --ignored`
//! exercises whichever backends this machine actually has. None of these
//! assert byte equality -- ffmpeg 9.0 and 7.1 (and ImageMagick 7.1 vs.
//! 6.9) do not produce byte-identical output from identical input, so
//! every assertion here is about a *property* of the result: dimensions,
//! codec, palette size, magic bytes.
//!
//! A missing backend must fail loudly and specifically. `require_backend`
//! is the one place that happens: it resolves against the real `Resolver`
//! (PATH, the managed install dir, well-known locations -- whatever this
//! machine has) and panics naming the backend and how to install it, so a
//! missing dependency is never mistaken for either a passing test or an
//! opaque `unwrap()` panic.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::{exec, registry, Backend, Format, Resolver};

// --- Fixtures ----------------------------------------------------------

/// `tests/fixtures/` at the repo root, resolved relative to this crate's
/// manifest directory so the tests work regardless of the process's
/// current directory.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn fixture(name: &str) -> PathBuf {
    let p = fixtures_dir().join(name);
    assert!(
        p.is_file(),
        "missing fixture tests/fixtures/{name}; see docs/defaults-calibration.md \
         and the Task 15 report for how each fixture was generated"
    );
    p
}

/// A private scratch directory for one helper call's output. Uses `keep()`
/// to deliberately leak the `TempDir` guard: the returned `PathBuf` needs
/// to outlive the helper call (the caller reads the file back afterward),
/// and these are short-lived, `--ignored`-gated test runs where a handful
/// of leftover temp directories cost nothing.
fn scratch_output(name: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix("convkit-output-properties-")
        .tempdir()
        .unwrap()
        .keep()
        .join(name)
}

// --- Backend resolution --------------------------------------------------

/// Resolves `backend` against the real, unmodified `Resolver` and panics
/// with a message naming the backend and how to install it if it isn't
/// found. Called before every real subprocess invocation below, so a
/// missing backend always fails the same clear way.
fn require_backend(resolver: &Resolver, backend: Backend) {
    if let Err(e) = resolver.resolve(backend) {
        let hint = e
            .remediation
            .as_ref()
            .and_then(|r| r.managed.as_deref().or(r.manual.as_deref()))
            .unwrap_or("no remediation available");
        panic!(
            "backend_missing: {} not found -- {hint}",
            backend.exe_name()
        );
    }
}

// --- Conversion helpers ----------------------------------------------------

/// Runs a real conversion through the same `exec::run` path `conv` uses in
/// production: real backend resolution, real subprocess, real scratch
/// directory. Never a stub. Pre-checks every backend the recipe needs via
/// `require_backend` so a missing one fails with a clear message before any
/// subprocess is even spawned.
fn convert_path(input: &Path, to_ext: &str) -> (PathBuf, exec::Outcome) {
    let from = Format::from_path(input)
        .unwrap_or_else(|| panic!("no known format for {}", input.display()));
    let to = Format::from_ext(to_ext).unwrap_or_else(|| panic!("no known format {to_ext:?}"));

    let resolver = Resolver::new();
    for backend in registry::backends_for(from, to) {
        require_backend(&resolver, backend);
    }

    let output = scratch_output(&format!("out.{to_ext}"));
    let req = exec::Request {
        from,
        to,
        inputs: vec![input.to_path_buf()],
        output: output.clone(),
    };
    let outcome = exec::run(&req, &resolver, &mut |_| {})
        .unwrap_or_else(|e| panic!("{} -> {to_ext} failed: {e}", input.display()));
    (output, outcome)
}

fn convert_fixture(name: &str, to_ext: &str) -> PathBuf {
    convert_path(&fixture(name), to_ext).0
}

/// Remuxes a fixture into a different container with a raw stream-copy
/// `ffmpeg` call -- not through convkit's own recipe table, which (by
/// design; see `registry.rs`'s `insert_media_family`) has no recipe that
/// targets `.mkv` at all. This exists purely to build test input for
/// `mkv_to_mp4_with_compatible_codecs_is_a_stream_copy`.
fn remux_fixture_to(name: &str, container_ext: &str) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let out = scratch_output(&format!("remuxed.{container_ext}"));
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(fixture(name))
        .args(["-c", "copy"])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "ffmpeg remux to {container_ext} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

// --- Inspection helpers ----------------------------------------------------

fn ffprobe_video_codec(path: &Path) -> String {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let ffprobe = resolver.resolve(Backend::Ffprobe).unwrap().path;
    let probe = convkit_core::probe::run(&ffprobe, path)
        .unwrap_or_else(|e| panic!("ffprobe failed on {}: {e}", path.display()));
    probe
        .video_codec
        .unwrap_or_else(|| panic!("no video stream in {}", path.display()))
}

/// `magick identify -format "%k" <file>[0]`. The `[0]` restricts
/// ImageMagick to the first frame: without it, a multi-frame file (an
/// animated GIF, or a burst-mode HEIC) makes `identify` emit one `%k` per
/// frame concatenated with no separator, which is not a single count.
fn imagemagick_unique_colors(path: &Path) -> u64 {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;

    let first_frame = format!("{}[0]", path.display());
    let out = Command::new(&magick)
        .args(["identify", "-format", "%k", &first_frame])
        .output()
        .unwrap_or_else(|e| panic!("failed to run magick identify: {e}"));
    assert!(
        out.status.success(),
        "magick identify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse()
        .unwrap_or_else(|_| panic!("could not parse unique-colour count from {text:?}"))
}

/// `magick identify -format "%w %h" <file>[0]`; see
/// `imagemagick_unique_colors` for why `[0]` matters.
fn imagemagick_dimensions(path: &Path) -> (u32, u32) {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;

    let first_frame = format!("{}[0]", path.display());
    let out = Command::new(&magick)
        .args(["identify", "-format", "%w %h", &first_frame])
        .output()
        .unwrap_or_else(|e| panic!("failed to run magick identify: {e}"));
    assert!(
        out.status.success(),
        "magick identify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let w: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse width from {text:?}"));
    let h: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse height from {text:?}"));
    (w, h)
}

// --- Properties --------------------------------------------------------

#[test]
#[ignore = "requires backends; run with --ignored"]
fn gif_output_uses_more_than_a_trivial_palette() {
    let out = convert_fixture("clip.mp4", "gif");
    let colors = imagemagick_unique_colors(&out);
    assert!(colors > 64, "palette collapsed to {colors} colours");
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn mkv_to_mp4_with_compatible_codecs_is_a_stream_copy() {
    let resolver = Resolver::new();
    // Not part of the mkv->mp4 recipe's own backend list -- `ffprobe` is
    // consulted transiently, before a recipe is even chosen -- but
    // essential to this test's own claim: without it, `plan::build`
    // conservatively transcodes instead of remuxing, and `outcome.remuxed`
    // below would fail for a confusing, unrelated-looking reason instead of
    // a clear missing-backend one.
    require_backend(&resolver, Backend::Ffprobe);

    let mkv = remux_fixture_to("clip.mp4", "mkv");
    let (out, outcome) = convert_path(&mkv, "mp4");
    assert!(outcome.remuxed, "should have stream-copied");
    assert_eq!(ffprobe_video_codec(&out), "h264");
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn heic_to_jpg_preserves_orientation_and_stays_reasonably_sized() {
    let heic = fixtures_dir().join("photo.heic");
    assert!(
        heic.is_file(),
        "missing fixture tests/fixtures/photo.heic -- HEIC encoders are scarce \
         and this repo will not fabricate a fake one. This machine's ffmpeg 9.0.x \
         build has no HEIC/HEIF muxer or demuxer at all (`ffmpeg -muxers` / \
         `-demuxers` confirm it), so it cannot be generated here either. Copy a \
         real photo off a recent iPhone (Camera defaults to HEIC under \
         Settings > Camera > Formats > High Efficiency) onto this machine, save \
         it as tests/fixtures/photo.heic (keep it under 200 KB -- a resized or \
         re-encoded HEIC is fine), commit it, and re-run with --ignored."
    );
    let out = convert_fixture("photo.heic", "jpg");
    let (w, h) = imagemagick_dimensions(&out);
    let (sw, sh) = imagemagick_dimensions(&heic);
    assert_eq!(
        (w, h),
        (sw, sh),
        "auto-orient must not transpose dimensions"
    );
    assert!(std::fs::metadata(&out).unwrap().len() > 1024);
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn docx_to_pdf_produces_a_real_pdf() {
    let out = convert_fixture("sample.docx", "pdf");
    let mut header = [0u8; 5];
    std::fs::File::open(&out)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    assert_eq!(&header, b"%PDF-");
}
