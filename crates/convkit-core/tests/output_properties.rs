//! Property tests against real backend output.
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

use convkit_core::budget::{MARGIN_PERMILLE, OVERHEAD_PERMILLE};
use convkit_core::{exec, registry, Backend, Format, MediaProbe, Resolver, Tuning};

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
         for how each fixture was generated"
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

/// A private scratch directory for one whole end-to-end test: a source
/// plus every output it produces, cleaned up automatically when the
/// returned `TempDir` drops at the end of the test function. Unlike
/// `scratch_output` above (which deliberately leaks a directory per call
/// because its caller reads the file back well after the helper returns),
/// the tests below build several files under one shared directory and
/// hold it for the test's whole lifetime, so a plain, non-leaking guard is
/// the right shape here.
fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("convkit-output-properties-e2e-")
        .tempdir()
        .unwrap()
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
    let output = scratch_output(&format!("out.{to_ext}"));
    let outcome = convert_tuned(input, &output, &Tuning::default())
        .unwrap_or_else(|e| panic!("{} -> {to_ext} failed: {e}", input.display()));
    (output, outcome)
}

/// `convert_path`'s sibling: the same real `exec::run` path, but for a
/// caller that already knows its own output path and wants a specific
/// `Tuning` honoured rather than the untuned default. `convert_path` is
/// expressed in terms of this rather than duplicating the `Request`
/// construction, so there is one code path building a conversion request
/// here, not two that can silently drift apart.
fn convert_tuned(
    input: &Path,
    output: &Path,
    tuning: &Tuning,
) -> convkit_core::Result<exec::Outcome> {
    let from = Format::from_path(input)
        .unwrap_or_else(|| panic!("no known format for {}", input.display()));
    let to = Format::from_path(output)
        .unwrap_or_else(|| panic!("no known format for {}", output.display()));

    let resolver = Resolver::new();
    for backend in registry::backends_for(from, to) {
        require_backend(&resolver, backend);
    }

    let req = exec::Request {
        from,
        to,
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite: false,
        tuning: tuning.clone(),
        allow_extreme: false,
    };
    exec::run(&req, &resolver, &mut |_| {})
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

/// Synthesises a `w`x`h` @ `fps` h264 source with `ffmpeg`'s `testsrc`
/// lavfi generator, written as `.mov` -- deliberately never `.mp4`: the
/// registry has no `mp4 -> mp4` pair (converting a container to itself
/// isn't a real conversion), and every video-knob test below targets
/// `.mp4`, so the source has to live in a different container to reach a
/// real `Mov -> Mp4` recipe. Two seconds is enough for a constant frame
/// rate to be unambiguous without making these backend-heavy `--ignored`
/// tests slow; `-preset ultrafast` keeps the encode itself cheap.
fn synth_video(dir: &tempfile::TempDir, w: u32, h: u32, fps: u32) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let out = dir.path().join("src.mov");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={w}x{h}:rate={fps}:duration=2"),
        ])
        .args([
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "building the synthetic video fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

/// `synth_video`'s 10-bit sibling: the same `testsrc` source, encoded
/// `yuv420p10le`/High 10 rather than the ordinary 8-bit `yuv420p` --
/// exactly the source `a_ten_bit_source_comes_out_eight_bit` needs:
/// without a forced `-pix_fmt yuv420p` on the tuned conversion, libx264
/// preserves this bit depth and emits High 10 straight through (verified
/// by hand against this machine's real ffmpeg while writing this test --
/// see that test's own docs).
fn synth_video_10bit(dir: &tempfile::TempDir, w: u32, h: u32, fps: u32) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let out = dir.path().join("src10.mov");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={w}x{h}:rate={fps}:duration=2"),
        ])
        .args([
            "-pix_fmt",
            "yuv420p10le",
            "-c:v",
            "libx264",
            "-profile:v",
            "high10",
            "-preset",
            "ultrafast",
        ])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "building the 10-bit synthetic video fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

/// An mkv source carrying two AAC audio tracks and one SubRip subtitle
/// track alongside its h264 video. Exists purely for
/// `a_transcode_keeps_the_tracks_the_static_recipe_would_drop`: the static
/// `VIDEO_TO_MP4` registry recipe hardcodes `-sn` (no subtitles at all)
/// and relies on ffmpeg's default stream selection (one video, one
/// audio), so only `media::transcoded_invocation`'s probe-driven mapping
/// -- reached because a video knob forces a re-encode -- can carry both
/// audio tracks and the subtitle through. Same construction approach as
/// `build_multi_stream_mp4_fixture` above, deliberately not through any
/// convkit-core recipe, for the same reason that function documents.
///
/// A tiny 64x64 frame, like `build_multi_stream_mp4_fixture`'s -- not
/// because the test cares about dimensions (only stream counts), but
/// because a needlessly large/long encode here bought nothing and cost a
/// great deal on a loaded machine: a full 1280x720@30fps two-second
/// version of this fixture was observed to make ffmpeg's newer threaded
/// scheduler stall for minutes under concurrent system load, while this
/// tiny version and the pre-existing sibling fixture never have. 30fps
/// (not the sibling's 10) so the test's own `--fps 24` genuinely caps
/// something and exercises the re-encode path its name promises, rather
/// than a rate the source was already under.
fn synth_mkv_two_audio_one_subtitle(dir: &tempfile::TempDir) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let srt = dir.path().join("fixture.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nhello\n").unwrap();

    let out = dir.path().join("src.mkv");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=30:duration=1"])
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
            "srt",
            "-shortest",
        ])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "building the two-audio/one-subtitle mkv fixture failed: {}",
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

/// `convkit_core::probe::run` against the real, resolved `ffprobe` --
/// exactly the probe `plan::build_tuned` itself consults when deciding how
/// to honour a video knob, so every property test below reads back what
/// the production code path actually based its own decisions on.
fn probe_media(path: &Path) -> MediaProbe {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let ffprobe = resolver.resolve(Backend::Ffprobe).unwrap().path;
    convkit_core::probe::run(&ffprobe, path)
        .unwrap_or_else(|e| panic!("ffprobe failed on {}: {e}", path.display()))
}

fn ffprobe_video_codec(path: &Path) -> String {
    probe_media(path)
        .video_codec
        .unwrap_or_else(|| panic!("no video stream in {}", path.display()))
}

/// The first video stream's frame rate, as `MediaProbe` reports it --
/// `(numerator, denominator)`, never a lossy float. See `MediaProbe::
/// frame_rate`'s own docs for why a rational.
fn probe_rate(path: &Path) -> (u32, u32) {
    probe_media(path)
        .frame_rate
        .unwrap_or_else(|| panic!("no frame rate for {}", path.display()))
}

/// The first video stream's stored `(width, height)`. Stored, not
/// displayed -- fine here, since none of these synthetic fixtures carry a
/// rotation; see `MediaProbe::display_dimensions` for the distinction that
/// would matter if one did.
fn probe_dims(path: &Path) -> (u32, u32) {
    let p = probe_media(path);
    (
        p.width
            .unwrap_or_else(|| panic!("no width for {}", path.display())),
        p.height
            .unwrap_or_else(|| panic!("no height for {}", path.display())),
    )
}

/// How many audio streams ffprobe sees, in the same order
/// `media::transcoded_invocation`'s own per-index mapping would count them.
fn probe_audio_count(path: &Path) -> usize {
    probe_media(path).audio_codecs.len()
}

/// How many subtitle streams ffprobe sees.
fn probe_subtitle_count(path: &Path) -> usize {
    probe_media(path).subtitle_codecs.len()
}

/// The first video stream's `pix_fmt`. Not part of `MediaProbe` -- core has
/// no need of it for any plan decision yet -- so this reads it straight
/// from raw `ffprobe -show_streams` JSON via `probe_streams_json` below,
/// the same helper the override-authority test elsewhere in this file
/// already uses for the same reason.
fn probe_pix_fmt(path: &Path) -> String {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let ffprobe = resolver.resolve(Backend::Ffprobe).unwrap().path;
    probe_streams_json(&ffprobe, path)
        .iter()
        .find(|s| s["codec_type"] == "video")
        .and_then(|s| s["pix_fmt"].as_str())
        .unwrap_or_else(|| panic!("no pix_fmt for {}", path.display()))
        .to_string()
}

/// Reads a GIF's global colour table straight out of its own bytes -- no
/// backend needed. Per the GIF87a/89a spec: signature, a 7-byte logical
/// screen descriptor, then -- when the packed byte's top bit is set -- a
/// table of `2^(size+1)` consecutive `(r, g, b)` triples, where `size` is
/// the packed byte's low 3 bits.
fn gif_global_color_table(path: &Path) -> Vec<(u8, u8, u8)> {
    let data =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert!(
        data.len() > 13 && &data[0..3] == b"GIF",
        "not a GIF: {}",
        path.display()
    );
    let packed = data[10];
    assert!(
        packed & 0x80 != 0,
        "GIF has no global colour table: {}",
        path.display()
    );
    let n = 1usize << (((packed & 0x07) as u32) + 1);
    let start = 13;
    let end = start + 3 * n;
    data[start..end]
        .chunks_exact(3)
        .map(|c| (c[0], c[1], c[2]))
        .collect()
}

/// ffmpeg's built-in GIF encoder, given no explicit palette at all, falls
/// back to a fixed, content-independent 256-entry "systematic" table: 8
/// levels each of red and green (step 36) by 4 levels of blue (step 85) --
/// entry `i` is `(36*(i%8), 36*((i/8)%8), 85*(i/64))`. Verified by hand
/// against this machine's real ffmpeg 9.0.1 while writing this test
/// (`ffmpeg -f lavfi -i testsrc=... plain.gif`, no palettegen involved at
/// all, reads back exactly this table). convkit's own GIF chain always
/// runs `palettegen`/`paletteuse` first (`TO_GIF_CHAIN` in registry.rs);
/// any GIF whose table matches this systematic formula exactly never went
/// through that chain -- the defect
/// `a_tuned_gif_still_has_an_optimised_palette` exists to catch, whether
/// the cause is the palette chain being dropped outright or a probe-driven
/// path bypassing the static GIF recipe altogether.
fn is_default_web_palette(path: &Path) -> bool {
    let table = gif_global_color_table(path);
    table.len() == 256
        && table.iter().enumerate().all(|(i, &(r, g, b))| {
            r == (36 * (i % 8)) as u8
                && g == (36 * ((i / 8) % 8)) as u8
                && b == (85 * (i / 64)) as u8
        })
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
/// The magick half of the same long-path guarantee, and the one the first
/// round missed: `IMG_TO_JPG` passes its input as `photo.heic[0]` -- the
/// frame selector rides on the token -- and that token was left out of the
/// positions the Windows rewriter is allowed to touch, so the most common
/// image conversions there are went on failing past `MAX_PATH` while every
/// other recipe was fixed.
///
/// The input lives in the deep directory too, not just the output: that is
/// what puts the selector token itself over the limit. Measured on Windows
/// 10 19045 with a 270-character input, magick reported `Input file does not
/// exist` before the fix and produced a byte-identical JPEG to the
/// short-path conversion after it.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn heic_to_jpg_succeeds_when_the_input_itself_approaches_max_path() {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);

    let base = tempfile::tempdir().unwrap();
    let deep_dir = make_deep_dir(base.path(), 240);

    let input = deep_dir.join("photo.heic");
    std::fs::copy(fixture("photo.heic"), &input)
        .unwrap_or_else(|e| panic!("failed to copy fixture into deep dir: {e}"));
    assert!(
        input.as_os_str().len() >= 248,
        "the input token is what must exceed the threshold here: {} ({} bytes)",
        input.display(),
        input.as_os_str().len()
    );
    let output = deep_dir.join("out.jpg");

    let req = exec::Request {
        from: Format::Heic,
        to: Format::Jpg,
        inputs: vec![input.clone()],
        output: output.clone(),
        overwrite: false,
        tuning: Default::default(),
        allow_extreme: false,
    };
    exec::run(&req, &resolver, &mut |_| {}).unwrap_or_else(|e| {
        panic!(
            "heic -> jpg from a {}-character input failed: {e}",
            input.as_os_str().len()
        )
    });

    let bytes = std::fs::metadata(&output)
        .unwrap_or_else(|e| panic!("no JPEG at {}: {e}", output.display()))
        .len();
    assert!(bytes > 0, "JPEG is empty: {}", output.display());
}

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
        tuning: Default::default(),
        allow_extreme: false,
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
        tuning: Default::default(),
        allow_extreme: false,
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
        tuning: Default::default(),
        allow_extreme: false,
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

// --- Properties: video knobs against real backends -----------------------
//
// Everything above this section asserts on argv: the command convkit built,
// never the file it produced. That is blind to a whole class of failure --
// a chain that is well-formed and wrong -- and two defects of exactly that
// shape shipped past a fully green suite on this branch: `--fps`/`--resize`
// reaching the rendered command for a transcode pair but not for any static
// recipe (so `video -> gif` accepted the flags, exited 0, and did nothing),
// and a tuned `mp4 -> mkv` re-encoding while still reporting "stream copy,
// no re-encode". The five tests below probe the actual output file instead.

#[test]
#[ignore = "requires backends; run with --ignored"]
fn fps_caps_and_never_raises() {
    let dir = tmp();
    let src = synth_video(&dir, 1280, 720, 30);
    let out = dir.path().join("capped.mp4");
    convert_tuned(
        &src,
        &out,
        &Tuning {
            fps: Some("15".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(probe_rate(&out), (15, 1));

    let up = dir.path().join("unchanged.mp4");
    convert_tuned(
        &src,
        &up,
        &Tuning {
            fps: Some("60".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(probe_rate(&up), (30, 1), "a cap must never raise a rate");
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn resize_caps_and_lands_on_even_dimensions() {
    let dir = tmp();
    let src = synth_video(&dir, 1280, 720, 30);
    for (geometry, expected) in [
        ("640x360", (640, 360)),
        ("801x", (800, 450)), // odd request, even result
        ("50%", (640, 360)),
        ("200%", (1280, 720)), // capped, not doubled
        ("4000x3000", (1280, 720)),
    ] {
        let out = dir
            .path()
            .join(format!("r{geometry}.mp4").replace(['%', 'x'], "_"));
        convert_tuned(
            &src,
            &out,
            &Tuning {
                resize: Some(geometry.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (w, h) = probe_dims(&out);
        assert_eq!((w, h), expected, "geometry {geometry}");
        assert_eq!((w % 2, h % 2), (0, 0), "libx264 rejects odd dimensions");
    }
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_ten_bit_source_comes_out_eight_bit() {
    // The failure no argv snapshot catches: without -pix_fmt yuv420p,
    // libx264 preserves the source and emits High 10, which a large
    // share of hardware decoders refuse.
    let dir = tmp();
    let src = synth_video_10bit(&dir, 1280, 720, 30);
    let out = dir.path().join("eight.mp4");
    convert_tuned(
        &src,
        &out,
        &Tuning {
            fps: Some("24".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(probe_pix_fmt(&out), "yuv420p");
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_transcode_keeps_the_tracks_the_static_recipe_would_drop() {
    let dir = tmp();
    let src = synth_mkv_two_audio_one_subtitle(&dir);
    let out = dir.path().join("kept.mp4");
    convert_tuned(
        &src,
        &out,
        &Tuning {
            fps: Some("24".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        probe_audio_count(&out),
        2,
        "the second audio track must survive"
    );
    assert_eq!(probe_subtitle_count(&out), 1);
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_tuned_gif_still_has_an_optimised_palette() {
    // Spliced after split[a][b] the value lands inside the palettegen
    // graph and produces a default web-palette GIF -- which still opens,
    // still animates, and looks wrong.
    let dir = tmp();
    let src = synth_video(&dir, 1280, 720, 30);
    let out = dir.path().join("tuned.gif");
    convert_tuned(
        &src,
        &out,
        &Tuning {
            fps: Some("10".into()),
            resize: Some("320x".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (w, _) = probe_dims(&out);
    assert_eq!(w, 320);
    assert_eq!(probe_rate(&out).0 / probe_rate(&out).1, 10);
    // A default web palette is exactly 256 evenly-spaced colours; an
    // optimised one is not.
    assert!(
        !is_default_web_palette(&out),
        "the palette chain was bypassed"
    );
}

// --- --max-size -----------------------------------------------------------

/// A clip noisy enough that the encoder has to spend the bits it is given:
/// a clean test pattern compresses so well that a bitrate target is never
/// reached, which would make "close to the target" untestable.
#[derive(Debug, Clone, Copy)]
struct Noisy {
    width: u32,
    height: u32,
    secs: u32,
    audio_tracks: usize,
    /// Maps every audio track ahead of the video, so the container's first
    /// stream is audio.
    audio_first: bool,
    /// The noise filter's strength. At 30 libx264 cannot hold a 1 MB rate
    /// at the picture first chosen for this clip, so the encode retries; at
    /// 10 it lands first time.
    strength: u32,
    /// The noise filter's seed, so that two clips differ; ffmpeg's own
    /// default when `None`.
    seed: Option<u32>,
}

impl Noisy {
    /// 1280x720, 6 s, one audio track after the video, and noise strong
    /// enough to make the encoder retry.
    fn hd() -> Noisy {
        Noisy {
            width: 1280,
            height: 720,
            secs: 6,
            audio_tracks: 1,
            audio_first: false,
            strength: 30,
            seed: None,
        }
    }

    fn synth(self, dir: &Path, name: &str) -> PathBuf {
        let Noisy {
            width: w,
            height: h,
            secs,
            audio_tracks,
            audio_first,
            strength,
            seed,
        } = self;
        let resolver = Resolver::new();
        require_backend(&resolver, Backend::Ffmpeg);
        let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
        let out = dir.join(name);
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error"])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size={w}x{h}:rate=30:duration={secs}"),
            ]);
        for t in 0..audio_tracks {
            cmd.args([
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency={}:duration={secs}", 440 + 110 * t),
            ]);
        }
        let seed = seed.map(|s| format!(":all_seed={s}")).unwrap_or_default();
        cmd.args([
            "-filter_complex",
            &format!("[0:v]noise=alls={strength}:allf=t{seed}[v]"),
        ]);
        if !audio_first {
            cmd.args(["-map", "[v]"]);
        }
        for t in 0..audio_tracks {
            cmd.args(["-map", &format!("{}:a", t + 1)]);
        }
        if audio_first {
            cmd.args(["-map", "[v]"]);
        }
        cmd.args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "12",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
        ])
        .arg(&out);
        let status = cmd.status().unwrap();
        assert!(status.success(), "synthesising {name}");
        out
    }
}

fn convert_sized(
    input: &Path,
    output: &Path,
    size: &str,
    allow_extreme: bool,
) -> convkit_core::Result<exec::Outcome> {
    convert_sized_with(input, output, size, Tuning::default(), allow_extreme)
}

/// `convert_sized` with other tuning beside the size.
fn convert_sized_with(
    input: &Path,
    output: &Path,
    size: &str,
    tuning: Tuning,
    allow_extreme: bool,
) -> convkit_core::Result<exec::Outcome> {
    let from = Format::from_path(input).unwrap();
    let to = Format::from_path(output).unwrap();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    require_backend(&resolver, Backend::Ffprobe);
    exec::run(
        &exec::Request {
            from,
            to,
            inputs: vec![input.to_path_buf()],
            output: output.to_path_buf(),
            overwrite: false,
            tuning: Tuning {
                max_size: Some(convkit_core::size::parse(size).unwrap()),
                ..tuning
            },
            allow_extreme,
        },
        &resolver,
        &mut |_| {},
    )
}

/// The result is at or under `target`. Fitted first time, it is no further
/// under it than the budget's own reserve (margin plus container overhead)
/// explains, with ten percentage points to spare for the encoder's rate
/// control; derived from the budget's constants so a recalibration moves
/// the test with it. A retried result was planned against a budget cut by
/// how far the earlier attempts ran over, and perhaps a smaller picture, so
/// it need only be at least half the target.
fn assert_close_under(bytes: u64, target: u64, attempts: u32) {
    assert!(bytes <= target, "{bytes} is over {target}");
    if attempts == 1 {
        let reserve = MARGIN_PERMILLE + OVERHEAD_PERMILLE + 100;
        let floor = target * 1000u64.saturating_sub(reserve) / 1000;
        assert!(
            bytes >= floor,
            "{bytes} is further under {target} than the {reserve} permille reserve allows \
             (floor {floor})"
        );
    } else {
        assert!(
            bytes >= target / 2,
            "{bytes} is under half of {target} after {attempts} attempts"
        );
    }
}

/// How many encode attempts a sized outcome took.
fn attempts(o: &exec::Outcome) -> u32 {
    o.sizing.as_ref().expect("a sized outcome").attempts
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn max_size_lands_under_the_target_and_close_to_it() {
    let dir = tmp();
    let src = Noisy::hd().synth(dir.path(), "src.mkv");
    let out = dir.path().join("small.mp4");
    let o = convert_sized(&src, &out, "1mb", false).unwrap();
    assert_close_under(
        std::fs::metadata(&out).unwrap().len(),
        1_000_000,
        attempts(&o),
    );
    assert!(!o.sizing.unwrap().over_target);
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn max_size_works_for_webm_too() {
    let dir = tmp();
    let src = Noisy::hd().synth(dir.path(), "src.mkv");
    let out = dir.path().join("small.webm");
    let o = convert_sized(&src, &out, "1mb", false).unwrap();
    assert_eq!(attempts(&o), 1, "{:?}", o.sizing);
    assert_close_under(std::fs::metadata(&out).unwrap().len(), 1_000_000, 1);
}

/// On a clip libx264 can hold to its rate, the first attempt lands close
/// under the target, with no retry: the budget's own accuracy, which the
/// noisier clips above cannot show because they always retry.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn max_size_lands_close_under_the_target_first_time_with_x264() {
    let dir = tmp();
    let src = Noisy {
        strength: 10,
        ..Noisy::hd()
    }
    .synth(dir.path(), "src.mkv");
    let out = dir.path().join("small.mp4");
    let o = convert_sized(&src, &out, "1mb", false).unwrap();
    assert_eq!(attempts(&o), 1, "{:?}", o.sizing);
    assert_close_under(std::fs::metadata(&out).unwrap().len(), 1_000_000, 1);
}

/// Every audio track survives a sized conversion, and the audio budget
/// counts all of them.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_three_track_mkv_keeps_every_track() {
    let dir = tmp();
    let src = Noisy {
        audio_tracks: 3,
        ..Noisy::hd()
    }
    .synth(dir.path(), "src.mkv");
    let out = dir.path().join("small.mkv");
    let o = convert_sized(&src, &out, "2mb", false).unwrap();
    assert_eq!(probe_audio_count(&out), 3);
    assert_close_under(
        std::fs::metadata(&out).unwrap().len(),
        2_000_000,
        attempts(&o),
    );
}

/// A source whose first stream is audio still gets its video rate applied
/// to the video, keeps its audio, and fits: nothing may assume the video is
/// stream 0.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_audio_first_mkv_is_sized_without_losing_its_pass_log() {
    let dir = tmp();
    let src = Noisy {
        audio_first: true,
        ..Noisy::hd()
    }
    .synth(dir.path(), "src.mkv");
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let ffprobe = resolver.resolve(Backend::Ffprobe).unwrap().path;
    let streams = probe_streams_json(&ffprobe, &src);
    assert_eq!(
        streams[0]["codec_type"], "audio",
        "the fixture must start with its audio track"
    );
    let out = dir.path().join("small.mkv");
    let o = convert_sized(&src, &out, "2mb", false).unwrap();
    assert_eq!(probe_audio_count(&out), 1);
    assert!(probe_media(&out).video_codec.is_some());
    assert_close_under(
        std::fs::metadata(&out).unwrap().len(),
        2_000_000,
        attempts(&o),
    );
    assert!(!o.sizing.unwrap().over_target);
}

/// Two jobs at once in one directory, whose name has a space, must not
/// share a pass log. The two sources are seeded apart, so statistics read
/// from the other job's log would not match the encode they guided.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn parallel_sized_jobs_keep_their_pass_logs_apart() {
    let dir = tmp();
    let spaced = dir.path().join("with space");
    std::fs::create_dir(&spaced).unwrap();
    let seeded = |seed| Noisy {
        seed: Some(seed),
        ..Noisy::hd()
    };
    let a = seeded(1).synth(&spaced, "a.mkv");
    let b = seeded(2).synth(&spaced, "b.mkv");
    assert_ne!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "the two sources must differ"
    );
    let outcomes = std::thread::scope(|s| {
        let ha = s.spawn(|| convert_sized(&a, &spaced.join("a.mp4"), "1mb", false));
        let hb = s.spawn(|| convert_sized(&b, &spaced.join("b.mp4"), "1mb", false));
        [ha.join().unwrap().unwrap(), hb.join().unwrap().unwrap()]
    });
    for (n, o) in ["a.mp4", "b.mp4"].into_iter().zip(&outcomes) {
        assert_close_under(
            std::fs::metadata(spaced.join(n)).unwrap().len(),
            1_000_000,
            attempts(o),
        );
    }
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_extreme_target_converts_only_when_allowed() {
    let dir = tmp();
    let src = Noisy::hd().synth(dir.path(), "src.mkv");
    let out = dir.path().join("tiny.mp4");
    let e = convert_sized(&src, &out, "30kb", false).unwrap_err();
    assert_eq!(e.code, convkit_core::ErrorCode::ConfirmationRequired);
    assert!(!out.exists());
    let o = convert_sized(&src, &out, "30kb", true).unwrap();
    assert!(out.is_file());
    assert!(
        o.notes
            .iter()
            .any(|n| n.starts_with("Extreme compression") || n.starts_with("Could not get under")),
        "{:?}",
        o.notes
    );
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_source_already_under_the_target_is_copied_untouched() {
    let dir = tmp();
    let src = Noisy {
        width: 640,
        height: 360,
        secs: 2,
        ..Noisy::hd()
    }
    .synth(dir.path(), "src.mkv");
    let out = dir.path().join("same.mkv");
    convert_sized(&src, &out, "50mb", false).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&src).unwrap());
}

/// A source already under the target must still be encoded when a
/// `--resize` binds or its codec does not suit the target; either way it is
/// budgeted at its own size, so the encode never makes it larger.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_source_already_under_the_target_is_not_grown_by_an_encode() {
    let dir = tmp();
    let noisy = Noisy {
        strength: 10,
        ..Noisy::hd()
    }
    .synth(dir.path(), "noisy.mkv");
    let small = dir.path().join("small.mp4");
    convert_sized(&noisy, &small, "1mb", false).unwrap();
    let source = std::fs::metadata(&small).unwrap().len();
    let resized = Tuning {
        resize: Some("640x".into()),
        ..Tuning::default()
    };
    for (name, tuning) in [("resized.mp4", resized), ("other.webm", Tuning::default())] {
        let out = dir.path().join(name);
        let o = convert_sized_with(&small, &out, "10mb", tuning, false).unwrap();
        let sizing = o.sizing.unwrap();
        assert_eq!(
            sizing.strategy,
            convkit_core::sized::Strategy::Encode,
            "{name}"
        );
        let bytes = std::fs::metadata(&out).unwrap().len();
        assert!(
            bytes <= source,
            "{name}: {bytes} bytes from a {source}-byte source"
        );
    }
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
