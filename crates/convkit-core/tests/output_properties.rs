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
        overwrite: false,
    };
    let outcome = exec::run(&req, &resolver, &mut |_| {})
        .unwrap_or_else(|e| panic!("{} -> {to_ext} failed: {e}", input.display()));
    (output, outcome)
}

fn convert_fixture(name: &str, to_ext: &str) -> PathBuf {
    convert_path(&fixture(name), to_ext).0
}

/// Remuxes a fixture into a different container with a raw stream-copy
/// `ffmpeg` call -- deliberately not through convkit's own `mp4 -> mkv`
/// recipe, even though the registry has carried one since mov/mkv became
/// conversion targets. Building this test's input via the very feature the
/// test exists to verify would make a break in *that* feature show up here
/// as a confusing fixture-setup failure instead of the clear assertion this
/// test is actually about. This exists purely to build test input for
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

/// Builds an mp4 fixture with two AAC audio tracks and a `mov_text`
/// subtitle stream -- mp4's only subtitle codec -- via a raw `ffmpeg`
/// invocation, deliberately not through any convkit-core recipe. Same
/// reasoning as `remux_fixture_to`: building this test's input via the
/// feature the test exists to verify would turn a break in *that* feature
/// into a confusing fixture-setup failure instead of the clear assertion
/// this test is actually about. Exists purely for
/// `mp4_to_mkv_with_no_probe_available_transcodes_and_preserves_every_stream`.
fn build_multi_stream_mp4_fixture(resolver: &Resolver) -> PathBuf {
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let srt = scratch_output("fixture.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nhello\n").unwrap();

    let out = scratch_output("multi_stream_src.mp4");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=1"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
        .args(["-f", "lavfi", "-i", "sine=frequency=880:duration=1"])
        .arg("-i")
        .arg(&srt)
        .args([
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:s",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-c:s",
            "mov_text",
            "-shortest",
        ])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "building the multi-stream mp4 fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

/// `ffprobe -show_streams` as parsed JSON, one `Value` per stream, in
/// stream-index order. Shared by the override-authority verification test
/// below for probing both its source fixture and its transcoded output.
fn probe_streams_json(ffprobe: &Path, path: &Path) -> Vec<serde_json::Value> {
    let out = Command::new(ffprobe)
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffprobe: {e}"));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("ffprobe produced invalid JSON: {e}"));
    v["streams"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("no streams in ffprobe output for {}", path.display()))
}

// --- Invocation choice ---------------------------------------------------

/// Chooses how to invoke ImageMagick's `identify` given `resolved` -- the
/// binary `Resolver::resolve(Backend::Magick)` actually resolved -- pinned
/// to the real, current platform. See `identify_command_for`'s docs for the
/// full IM6/IM7 rationale.
fn identify_command(resolved: &Path) -> (PathBuf, Vec<String>) {
    identify_command_for(resolved, cfg!(windows))
}

/// The logic behind `identify_command`. ImageMagick 7 ships one unified
/// `magick` binary and treats companion tools (`identify`, `mogrify`,
/// `compare`, `-list`, ...) as subcommands: `magick identify ...`.
/// ImageMagick 6 -- what `Resolver`'s `convert` fallback resolves to on a
/// machine whose package manager still ships IM6, e.g. Ubuntu's
/// `apt-get install imagemagick` -- has no such subcommand: `identify` is
/// its own binary, sitting beside `convert` in the same directory. Running
/// `<IM6 convert> identify ...` doesn't fail to find the subcommand; it
/// happily tries to *convert a file named `identify`*, which is the bug
/// this function exists to prevent (this file only ever needs `identify`,
/// not `mogrify`/`compare`/`-list`, but the same choice would apply to
/// those).
///
/// Decided purely from `resolved`'s file name: `magick` (IM7) means invoke
/// `<resolved> identify ...`; anything else (IM6's `convert`) means invoke
/// the sibling `identify` binary in `resolved`'s own directory, with no
/// subcommand -- the resolved path's parent directory, not `PATH`, per how
/// the rest of this project treats resolved backends (see `cli.rs`'s
/// `ffmpeg_path`-derived `ffprobe` sibling lookup).
///
/// Takes `is_windows` as an explicit argument rather than reading
/// `cfg!(windows)` itself -- mirroring `resolve.rs`'s
/// `magick_convert_fallback_applies` -- so the executable-extension choice
/// below is testable for *both* platforms' convention on every host in
/// CI's `cargo test --workspace` matrix (ubuntu-latest, macos-latest,
/// windows-latest), including windows-latest, where `cfg!(windows)` is
/// always `true` and could otherwise never exercise the non-Windows branch.
///
/// The file name is located by hand -- the last `/` or `\`, whichever is
/// later in the string -- rather than through `std::path::Path`'s
/// component parser, whose separator handling is host-specific (`\` is not
/// a path separator on non-Windows). Every real `resolved` path is native
/// to the host that produced it, so this makes no difference there; it's
/// what lets this function's own unit tests exercise both a Windows-style
/// and a Unix-style path deterministically regardless of which OS is
/// actually running the test.
fn identify_command_for(resolved: &Path, is_windows: bool) -> (PathBuf, Vec<String>) {
    let raw = resolved.to_string_lossy().into_owned();
    let split = raw.rfind(['/', '\\']);
    let dir = match split {
        Some(idx) => &raw[..=idx],
        None => "",
    };
    let file_name = match split {
        Some(idx) => &raw[idx + 1..],
        None => raw.as_str(),
    };
    let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);

    if stem == "magick" {
        (resolved.to_path_buf(), vec!["identify".to_string()])
    } else {
        let sibling = if is_windows {
            "identify.exe"
        } else {
            "identify"
        };
        (PathBuf::from(format!("{dir}{sibling}")), Vec::new())
    }
}

/// Runs `identify_command(magick)` with `args` appended, panicking with the
/// captured stderr on a non-zero exit. Shared by `imagemagick_unique_colors`
/// and `imagemagick_dimensions`, whose only difference is the `-format`
/// string.
fn run_identify(magick: &Path, args: &[&str]) -> String {
    let (bin, mut full_args) = identify_command(magick);
    full_args.extend(args.iter().map(|s| s.to_string()));
    let out = Command::new(&bin)
        .args(&full_args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run identify ({}): {e}", bin.display()));
    assert!(
        out.status.success(),
        "identify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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

/// `identify -format "%k" <file>[0]` (see `identify_command` for how the
/// binary and any subcommand are chosen). The `[0]` restricts ImageMagick
/// to the first frame: without it, a multi-frame file (an animated GIF, or
/// a burst-mode HEIC) makes `identify` emit one `%k` per frame concatenated
/// with no separator, which is not a single count.
fn imagemagick_unique_colors(path: &Path) -> u64 {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;

    let first_frame = format!("{}[0]", path.display());
    let text = run_identify(&magick, &["-format", "%k", &first_frame]);
    text.trim()
        .parse()
        .unwrap_or_else(|_| panic!("could not parse unique-colour count from {text:?}"))
}

/// `identify -format "%w %h" <file>[0]`; see `imagemagick_unique_colors`
/// for why `[0]` matters and `identify_command` for how the binary and any
/// subcommand are chosen.
fn imagemagick_dimensions(path: &Path) -> (u32, u32) {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;

    let first_frame = format!("{}[0]", path.display());
    let text = run_identify(&magick, &["-format", "%w %h", &first_frame]);
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
        "missing fixture tests/fixtures/photo.heic -- a real one (1.58 MB) is \
         normally committed at this path, so if you're seeing this, it has been \
         removed or you're on a shallow/sparse checkout. HEIC encoders are \
         scarce -- this repo will not fabricate a fake one -- and no HEIC \
         encoder is available in any toolchain used here either (ImageMagick's \
         HEIC support is read-only, `magick -list format` reports `HEIC` as \
         `r--`, and no ffmpeg build available has a HEIC muxer), which is why \
         the committed fixture is a full-size 1.58 MB real photo rather than a \
         shrunk one. Copy a real photo off a recent iPhone (Camera defaults to \
         HEIC under Settings > Camera > Formats > High Efficiency) onto this \
         machine, save it as tests/fixtures/photo.heic, commit it, and re-run \
         with --ignored. Smaller is preferred if you can produce it with a \
         real HEIC encoder, but 1.58 MB is the honest floor without one."
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

/// Builds a directory whose full path is at least `min_len` bytes long by
/// repeatedly appending a fixed-length nested subdirectory under `base`,
/// then creates it. Every segment name is well under the ~255-byte
/// component limit every mainstream filesystem enforces, so this genuinely
/// gets *deeper*, not just longer components -- meaningful on Linux and
/// macOS too, not only on Windows where `MAX_PATH` (260 bytes) is a real,
/// enforced ceiling. `Path::join` throughout means the result uses each
/// platform's own native separator.
fn make_deep_dir(base: &Path, min_len: usize) -> PathBuf {
    let segment = "convkit_deep_destination_directory_segment";
    let mut dir = base.to_path_buf();
    while dir.as_os_str().len() < min_len {
        dir = dir.join(segment);
    }
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "failed to create deep test directory {}: {e}",
            dir.display()
        )
    });
    dir
}

/// The regression test for the LibreOffice-profile-path fix (see the fix's
/// own report). Before it, `exec::run` created the `-env:UserInstallation`
/// profile *inside* the per-conversion scratch directory, which itself
/// lives inside the user's chosen destination directory -- so LibreOffice's
/// own, fairly deep profile tree (`user/config/...` and more) ended up
/// nested under whatever the destination path already was. On Windows that
/// routinely blew past the 260-character `MAX_PATH`: `soffice` still
/// exited 0 (see `a_backend_that_writes_nothing_is_a_failure_even_on_exit_
/// zero` in `exec.rs` for the general shape of that trap), but produced no
/// PDF, and the only visible symptom was LibreOffice's own "the
/// configuration file ... bootstrap.ini is corrupt" popup -- a message that
/// points at the LibreOffice installation, not at convkit's own path
/// choice.
///
/// This drives a real `docx -> pdf` conversion, through the exact same
/// `exec::run` path `conv` uses in production, with *both* the input and
/// the output sitting in a destination directory built to be at least 150
/// characters deep -- comfortably past what the old, buggy code needed to
/// fail on this project's own controlled experiment (a 151-character
/// destination was already enough). The directory lives inside a
/// `tempfile::tempdir()` so it cleans itself up regardless of outcome.
/// F193, the half the LibreOffice-profile fix above did not reach. Moving
/// the profile out of the destination left the scratch directory and every
/// step intermediate still nested inside it, so a destination a user could
/// legitimately have -- around 240 characters -- still pushed the path
/// backends actually receive past `MAX_PATH`. They failed naming the wrong
/// cause: soffice with `no export filter for  found`, magick with `unable to
/// open image`, neither mentioning length.
///
/// `md -> pdf` is the sharpest case available without magick installed: two
/// steps, so it covers an intermediate path as well as a final one, and its
/// second step is soffice, which is one of the two backends that is not long
/// path aware. Measured on Windows 10 19045 with a 432-character
/// destination: `md -> pdf` and `docx -> pdf` both failed before this fix
/// and both succeed after, while `mp4 -> gif` succeeded either way -- ffmpeg
/// having no such limit -- which is what makes this a fix rather than a
/// workaround for one backend.
///
/// 240 rather than 150: the profile bug fired at 150, but the destination
/// side needs the path itself to approach 260 before a backend refuses it.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn md_to_pdf_succeeds_in_a_destination_that_approaches_max_path() {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Pandoc);
    require_backend(&resolver, Backend::Soffice);

    let base = tempfile::tempdir().unwrap();
    let deep_dir = make_deep_dir(base.path(), 240);

    let input = deep_dir.join("sample.md");
    std::fs::write(&input, "# Title\n\nBody text.\n").unwrap();
    let output = deep_dir.join("out.pdf");
    assert!(
        output.as_os_str().len() >= 248,
        "test setup must exceed the verbatim threshold: {} ({} bytes)",
        output.display(),
        output.as_os_str().len()
    );

    let req = exec::Request {
        from: Format::Md,
        to: Format::Pdf,
        inputs: vec![input],
        output: output.clone(),
        overwrite: false,
    };
    exec::run(&req, &resolver, &mut |_| {}).unwrap_or_else(|e| {
        panic!(
            "md -> pdf into a {}-character destination failed: {e}",
            output.as_os_str().len()
        )
    });

    let bytes = std::fs::metadata(&output)
        .unwrap_or_else(|e| panic!("no PDF at {}: {e}", output.display()))
        .len();
    assert!(bytes > 0, "PDF is empty: {}", output.display());
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn docx_to_pdf_succeeds_in_a_deep_destination_directory() {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Soffice);

    let base = tempfile::tempdir().unwrap();
    let deep_dir = make_deep_dir(base.path(), 150);
    assert!(
        deep_dir.as_os_str().len() >= 150,
        "test setup failed to build a deep enough directory: {} ({} bytes)",
        deep_dir.display(),
        deep_dir.as_os_str().len()
    );

    let input = deep_dir.join("sample.docx");
    std::fs::copy(fixture("sample.docx"), &input)
        .unwrap_or_else(|e| panic!("failed to copy fixture into deep dir: {e}"));
    let output = deep_dir.join("out.pdf");

    let req = exec::Request {
        from: Format::Docx,
        to: Format::Pdf,
        inputs: vec![input],
        output: output.clone(),
        overwrite: false,
    };
    exec::run(&req, &resolver, &mut |_| {})
        .unwrap_or_else(|e| panic!("docx -> pdf into a deep destination directory failed: {e}"));

    let mut header = [0u8; 5];
    std::fs::File::open(&output)
        .unwrap_or_else(|e| panic!("expected a real output file at {}: {e}", output.display()))
        .read_exact(&mut header)
        .unwrap();
    assert_eq!(&header, b"%PDF-", "output is not a real PDF");
}

/// Override-authority fix verification (see the fix's own report). The
/// coordinator's own attempt to prove this could never force ffprobe
/// genuinely unavailable: a plain `--ffprobe-path <nonexistent>` used to
/// fall through to a real ffprobe found elsewhere (the exact defect the fix
/// closes), so probing silently succeeded and the *remux* path ran instead
/// of the transcode path this test is actually about.
/// `Resolver::overrides_only()` with an override supplied for ffmpeg but
/// *none* for ffprobe closes that gap: `candidates()` is empty for ffprobe
/// deterministically (see `overrides_only`'s docs in `resolve.rs`), so
/// probing is genuinely unavailable while ffmpeg still resolves normally.
///
/// Proves the whole point end to end: `mp4 -> mkv` on the no-probe
/// transcode path (`video_to_mkv_recipe_for` in `registry.rs` picks
/// `VIDEO_TO_MKV_SRT_SUBS` for an mp4 source purely from the container
/// type, with no probe involved at all) must carry every stream through --
/// video, both audio tracks, and the subtitle -- not just the first of
/// each the way the mp4-*target* recipes do. Video and audio must also
/// show clear evidence of a genuine re-encode, not a stream copy dressed up
/// as one: `pix_fmt` changes from the source's `yuv444p` to the recipe's
/// forced `yuv420p`, and every re-encoded stream picks up ffmpeg's own
/// per-stream `ENCODER` tag when writing to Matroska -- something the
/// source's own *audio* streams do not carry at all (only its
/// already-once-encoded video stream does), so its appearance on the
/// transcoded audio streams is itself proof they passed through an encoder
/// here, not a demuxer/muxer copy.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn mp4_to_mkv_with_no_probe_available_transcodes_and_preserves_every_stream() {
    let real = Resolver::new();
    require_backend(&real, Backend::Ffmpeg);
    require_backend(&real, Backend::Ffprobe);
    let ffmpeg = real.resolve(Backend::Ffmpeg).unwrap().path;
    let ffprobe = real.resolve(Backend::Ffprobe).unwrap().path;

    let src = build_multi_stream_mp4_fixture(&real);

    // Force the no-probe path: ffmpeg via an explicit override, no override
    // at all for ffprobe -- overrides_only makes candidates() empty for it,
    // so resolve(Ffprobe) genuinely fails and exec::run's `.ok()` on that
    // result falls back to `probed: None`, the same as a machine with no
    // ffprobe installed at all.
    let mut forced = Resolver::new();
    forced.overrides_only();
    forced.with_override(Backend::Ffmpeg, ffmpeg.clone());
    assert!(
        forced.resolve(Backend::Ffprobe).is_err(),
        "ffprobe must be genuinely unresolvable through this Resolver"
    );

    let out = scratch_output("transcoded.mkv");
    let req = exec::Request {
        from: Format::Mp4,
        to: Format::Mkv,
        inputs: vec![src.clone()],
        output: out.clone(),
        overwrite: false,
    };
    let outcome = exec::run(&req, &forced, &mut |_| {})
        .unwrap_or_else(|e| panic!("mp4 -> mkv transcode failed: {e}"));
    assert!(
        !outcome.remuxed,
        "with no probe available, this must transcode, not remux"
    );

    // Probe both the source and the transcoded output with the real,
    // unrestricted resolver's ffprobe -- proving what's actually in the
    // files, not just what the recipe intended.
    let src_streams = probe_streams_json(&ffprobe, &src);
    let out_streams = probe_streams_json(&ffprobe, &out);

    let codec_names: Vec<&str> = out_streams
        .iter()
        .map(|s| s["codec_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        codec_names,
        vec!["h264", "aac", "aac", "subrip"],
        "every stream must survive the transcode, in order: {out_streams:#?}"
    );

    // Video: pix_fmt changed from the source's yuv444p to the recipe's
    // forced yuv420p -- only possible via a genuine re-encode.
    assert_eq!(src_streams[0]["pix_fmt"], "yuv444p", "{src_streams:#?}");
    assert_eq!(out_streams[0]["pix_fmt"], "yuv420p", "{out_streams:#?}");
    let video_encoder = out_streams[0]["tags"]["ENCODER"].as_str().unwrap_or("");
    assert!(
        video_encoder.contains("libx264"),
        "video stream must carry a libx264 ENCODER tag: {video_encoder:?}"
    );

    // Audio: the source's own audio streams carry no ENCODER tag at all
    // (only its once-already-encoded video stream does); its appearance on
    // both transcoded audio streams is itself the proof they passed
    // through ffmpeg's aac encoder here, not a stream copy.
    assert!(
        src_streams[1]["tags"].get("ENCODER").is_none(),
        "sanity check on the fixture: {src_streams:#?}"
    );
    for i in [1, 2] {
        let encoder = out_streams[i]["tags"]["ENCODER"].as_str().unwrap_or("");
        assert!(
            encoder.contains("aac"),
            "audio stream {i} must carry an aac ENCODER tag: {encoder:?}"
        );
    }

    // Subtitle: mov_text (the source) became subrip (the recipe's forced
    // -c:s srt) -- text re-encoded to a different text codec, matroska's
    // own well-supported one, not the mov_text matroska has no codec for.
    assert_eq!(src_streams[3]["codec_name"], "mov_text", "{src_streams:#?}");
}

// --- Unit tests: identify_command ------------------------------------------
//
// Not `#[ignore]`d and not gated on any installed backend: this covers the
// IM6/IM7 invocation-choice logic itself as a pure function of a path
// string, so it runs (and can fail for real) on every machine, including
// this one, which only has ImageMagick 7 -- the same reason the property
// tests above exist for the recipes themselves, applied to a test helper.
mod tests {
    use super::*;

    #[test]
    fn magick_windows_style_path_is_a_subcommand_on_both_platforms() {
        let resolved = Path::new(r"C:\Program Files\ImageMagick-7.1.2-Q16-HDRI\magick.exe");
        for is_windows in [true, false] {
            let (bin, args) = identify_command_for(resolved, is_windows);
            assert_eq!(bin, resolved, "is_windows={is_windows}");
            assert_eq!(
                args,
                vec!["identify".to_string()],
                "is_windows={is_windows}"
            );
        }
    }

    #[test]
    fn magick_unix_style_path_is_a_subcommand_on_both_platforms() {
        let resolved = Path::new("/usr/local/bin/magick");
        for is_windows in [true, false] {
            let (bin, args) = identify_command_for(resolved, is_windows);
            assert_eq!(bin, resolved, "is_windows={is_windows}");
            assert_eq!(
                args,
                vec!["identify".to_string()],
                "is_windows={is_windows}"
            );
        }
    }

    #[test]
    fn convert_windows_style_path_uses_a_sibling_identify_binary() {
        let resolved = Path::new(r"C:\Program Files\ImageMagick-6.9-Q16\convert.exe");

        let (bin, args) = identify_command_for(resolved, true);
        assert_eq!(
            bin,
            Path::new(r"C:\Program Files\ImageMagick-6.9-Q16\identify.exe")
        );
        assert!(args.is_empty());

        let (bin, args) = identify_command_for(resolved, false);
        assert_eq!(
            bin,
            Path::new(r"C:\Program Files\ImageMagick-6.9-Q16\identify")
        );
        assert!(args.is_empty());
    }

    #[test]
    fn convert_unix_style_path_uses_a_sibling_identify_binary() {
        let resolved = Path::new("/usr/bin/convert");

        let (bin, args) = identify_command_for(resolved, true);
        assert_eq!(bin, Path::new("/usr/bin/identify.exe"));
        assert!(args.is_empty());

        let (bin, args) = identify_command_for(resolved, false);
        assert_eq!(bin, Path::new("/usr/bin/identify"));
        assert!(args.is_empty());
    }

    /// `identify_command` (no explicit `is_windows`) must agree with
    /// `identify_command_for` pinned to the real, current platform -- this
    /// is what every real call site actually uses.
    #[test]
    fn identify_command_matches_the_real_platform() {
        let resolved = Path::new("/usr/bin/convert");
        assert_eq!(
            identify_command(resolved),
            identify_command_for(resolved, cfg!(windows))
        );
    }
}
