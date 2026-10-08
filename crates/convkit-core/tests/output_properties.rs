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
    convert_tuned_with(input, output, tuning, false)
}

/// `convert_tuned`, answering yes to a question the run asks first.
fn convert_tuned_with(
    input: &Path,
    output: &Path,
    tuning: &Tuning,
    allow_extreme: bool,
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
        allow_extreme,
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

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_gif_fps_at_or_above_the_source_keeps_the_source_rate() {
    // GIF authors its own 15 fps. A `--fps` the source already satisfies
    // must replace that default with the source's rate, not fall back to
    // it: before, `--fps 29` on this source gave 29 fps and `--fps 30` gave
    // 15.
    let dir = tmp();
    let src = synth_video(&dir, 320, 180, 30);
    for fps in ["29", "30", "60"] {
        let out = dir.path().join(format!("f{fps}.gif"));
        convert_tuned(
            &src,
            &out,
            &Tuning {
                fps: Some(fps.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (n, d) = probe_rate(&out);
        let expected = if fps == "29" { 29 } else { 30 };
        assert_eq!((n / d, n % d), (expected, 0), "--fps {fps}");
    }
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_resize_that_does_not_bind_keeps_the_stream_copy() {
    // A size the source already fits within changes nothing, so it must
    // not cost a re-encode: before, it gave up the copy for an identical
    // picture.
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let mkv = remux_fixture_to("clip.mp4", "mkv");
    let dir = tmp();
    let out = dir.path().join("kept.mp4");
    let outcome = convert_tuned(
        &mkv,
        &out,
        &Tuning {
            resize: Some("8000x".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(outcome.remuxed, "should have stream-copied");
    assert_eq!(probe_dims(&out), probe_dims(&mkv));
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_gif_resize_past_the_source_keeps_the_source_size() {
    // Not the recipe's 640 default, and not enlarged.
    let dir = tmp();
    let src = synth_video(&dir, 320, 180, 30);
    let out = dir.path().join("kept.gif");
    convert_tuned(
        &src,
        &out,
        &Tuning {
            resize: Some("4000x".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(probe_dims(&out), (320, 180));
}

// --- Notes that depend on the source ----------------------------------------

/// Writes `out` in `dir` with ImageMagick, from `args`.
fn synth_image(dir: &tempfile::TempDir, args: &[&str], out: &str) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;
    let path = dir.path().join(out);
    let result = Command::new(&magick)
        .args(args)
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ImageMagick: {e}"));
    assert!(
        result.status.success(),
        "building {out} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    path
}

/// A phone photo gets no note; a transparent PNG the alpha half; a
/// multi-page TIFF the frame half, plus the alpha half, which a TIFF's
/// header read cannot rule out.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn image_notes_say_only_what_the_source_holds() {
    let said = |o: &exec::Outcome, what: &str| o.warnings.iter().any(|w| w.contains(what));
    let (_, photo) = convert_path(&fixture("photo.heic"), "jpg");
    assert!(photo.warnings.is_empty(), "{:?}", photo.warnings);

    let dir = tmp();
    let transparent = synth_image(
        &dir,
        &[
            "-size",
            "64x64",
            "xc:none",
            "-fill",
            "red",
            "-draw",
            "circle 32,32 32,8",
        ],
        "transparent.png",
    );
    let (_, o) = convert_path(&transparent, "jpg");
    assert!(said(&o, "Transparency"), "{:?}", o.warnings);
    assert!(!said(&o, "first frame"), "{:?}", o.warnings);

    let pages = synth_image(&dir, &["-size", "32x32", "xc:red", "xc:blue"], "pages.tiff");
    let (_, o) = convert_path(&pages, "png");
    assert!(said(&o, "first frame"), "{:?}", o.warnings);
    let (_, o) = convert_path(&pages, "jpg");
    assert!(said(&o, "first frame"), "{:?}", o.warnings);
    assert!(said(&o, "Transparency"), "{:?}", o.warnings);

    let opaque = synth_image(&dir, &["-size", "32x32", "xc:red"], "opaque.png");
    let (_, o) = convert_path(&opaque, "bmp");
    assert!(o.warnings.is_empty(), "{:?}", o.warnings);
}

/// The GIF buffering note is for a source past 30 seconds.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn the_gif_buffering_note_is_for_long_sources_only() {
    let (_, short) = convert_path(&fixture("clip.mp4"), "gif");
    assert!(short.warnings.is_empty(), "{:?}", short.warnings);

    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let dir = tmp();
    let long = dir.path().join("long.mp4");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=15:duration=31",
        ])
        .args([
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .arg(&long)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(result.status.success(), "{result:?}");
    let (_, o) = convert_path(&long, "gif");
    assert_eq!(o.warnings.len(), 1, "{:?}", o.warnings);
    assert!(
        o.warnings[0].contains("buffered in memory"),
        "{:?}",
        o.warnings
    );
}

// --- CSV and workbooks ----------------------------------------------------

/// Runs LibreOffice directly, as `synth_media` runs ffmpeg, to build a test
/// source. Its own profile, so it never collides with a running
/// LibreOffice.
fn soffice_build(dir: &tempfile::TempDir, input: &Path, filter: &str, ext: &str) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Soffice);
    let soffice = resolver.resolve(Backend::Soffice).unwrap().path;
    let profile = tempfile::tempdir().unwrap();
    let url = format!(
        "file:///{}",
        profile
            .path()
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let out_dir = dir.path().join(format!("built-{ext}"));
    let result = Command::new(&soffice)
        .arg(format!("-env:UserInstallation={url}"))
        .args([
            "--headless",
            "--norestore",
            "--convert-to",
            filter,
            "--outdir",
        ])
        .arg(&out_dir)
        .arg(input)
        .output()
        .unwrap_or_else(|e| panic!("failed to run soffice: {e}"));
    let built = out_dir.join(input.with_extension(ext).file_name().unwrap());
    assert!(
        built.is_file(),
        "building {} failed: {}",
        built.display(),
        String::from_utf8_lossy(&result.stdout)
    );
    built
}

fn write_bytes(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// One file inside a zip, as text.
fn zip_text(path: &Path, name: &str) -> String {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name(name)
        .unwrap_or_else(|e| panic!("{name} in {}: {e}", path.display()))
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// A cell of an .xlsx, as Excel will see it.
#[derive(Debug, PartialEq)]
enum Cell {
    Text(String),
    Number(f64),
}

/// The cells of an .xlsx's first sheet by reference (`A2`), read from its
/// XML: a shared string is text, anything else with a value a number.
fn xlsx_cells(path: &Path) -> std::collections::BTreeMap<String, Cell> {
    let unescape = |s: &str| {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
    };
    let between = |s: &str, open: &str, close: &str| -> Option<String> {
        let start = s.find(open)? + open.len();
        let len = s[start..].find(close)?;
        Some(s[start..start + len].to_string())
    };
    let shared: Vec<String> = zip_text(path, "xl/sharedStrings.xml")
        .split("<si>")
        .skip(1)
        .map(|si| {
            let si = &si[..si.find("</si>").unwrap()];
            let mut text = String::new();
            for part in si.split("<t").skip(1) {
                let body = &part[part.find('>').unwrap() + 1..];
                text.push_str(&unescape(&body[..body.find("</t>").unwrap()]));
            }
            text
        })
        .collect();
    zip_text(path, "xl/worksheets/sheet1.xml")
        .split("<c ")
        .skip(1)
        .filter_map(|c| {
            let open = &c[..c.find('>')?];
            let reference = between(open, "r=\"", "\"")?;
            let value = between(c, "<v>", "</v>")?;
            let cell = if open.contains("t=\"s\"") {
                Cell::Text(shared[value.parse::<usize>().unwrap()].clone())
            } else {
                Cell::Number(value.parse().unwrap())
            };
            Some((reference, cell))
        })
        .collect()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

/// The CSV every import test reads: a zip code with a leading zero, a
/// quantity, an ISO date, a fraction-looking ratio, a formula and a card
/// number.
const ORDERS_CSV: &str = "zip,qty,shipped,ratio,note,card\n\
                          02134,3,2024-01-15,1/2,=1+1,4111111111111111\n\
                          10001,12,2024-02-01,3-4,ok,12\n";

/// Leading zeros and long numbers survive as text, numbers and ISO dates
/// are numbers, and nothing is turned into a date or run as a formula.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_csv_keeps_its_digits_and_types_the_rest() {
    let dir = tmp();
    let csv = write_bytes(&dir, "orders.csv", ORDERS_CSV.as_bytes());
    let (xlsx, o) = convert_path(&csv, "xlsx");
    let cells = xlsx_cells(&xlsx);
    assert_eq!(cells["A2"], text("02134"));
    assert_eq!(cells["A3"], text("10001"), "the whole column is text");
    assert_eq!(cells["B2"], Cell::Number(3.0));
    assert_eq!(cells["C2"], Cell::Number(45306.0), "2024-01-15 is a date");
    assert_eq!(cells["D2"], text("1/2"), "not the 2nd of January");
    assert_eq!(cells["E2"], text("=1+1"), "not run");
    assert_eq!(cells["F2"], text("4111111111111111"), "every digit");
    assert_eq!(cells["F3"], text("12"));
    assert!(!cells.contains_key("A4"), "three rows: {cells:?}");
    assert!(
        o.warnings
            .iter()
            .any(|w| w.starts_with("The zip and card columns are kept as text")),
        "{:?}",
        o.warnings
    );
    assert!(
        o.warnings
            .iter()
            .any(|w| w.contains("formulas in a CSV are not run")),
        "{:?}",
        o.warnings
    );
}

/// A semicolon-separated Windows-1252 file with decimal commas, as a
/// European Excel writes it, and a UTF-16 one with tabs, as Excel's
/// "Unicode Text" writes it.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_csv_is_read_in_the_encoding_and_separators_it_was_written_in() {
    let dir = tmp();
    let euro = write_bytes(
        &dir,
        "prices.csv",
        b"name;price\nCaf\xe9;1,5\nTh\xe9;12,25\n",
    );
    let (xlsx, o) = convert_path(&euro, "xlsx");
    let cells = xlsx_cells(&xlsx);
    assert_eq!(cells["A2"], text("Café"));
    assert_eq!(cells["B2"], Cell::Number(1.5));
    assert_eq!(cells["B3"], Cell::Number(12.25));
    assert_eq!(o.warnings.len(), 2, "{:?}", o.warnings);

    let mut wide = vec![0xFF, 0xFE];
    wide.extend(
        "id\tname\n007\tZoë\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let wide = write_bytes(&dir, "wide.csv", &wide);
    let cells = xlsx_cells(&convert_path(&wide, "xlsx").0);
    assert_eq!(cells["A2"], text("007"));
    assert_eq!(cells["B2"], text("Zoë"));

    let bom = write_bytes(&dir, "bom.csv", b"\xEF\xBB\xBFname\nok\n");
    let cells = xlsx_cells(&convert_path(&bom, "xlsx").0);
    assert_eq!(cells["A1"], text("name"), "the byte-order mark is not data");
}

/// The same import into .ods: the zip code is a string cell.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_csv_to_ods_keeps_its_digits_too() {
    let dir = tmp();
    let csv = write_bytes(&dir, "orders.csv", ORDERS_CSV.as_bytes());
    let (ods, _) = convert_path(&csv, "ods");
    let content = zip_text(&ods, "content.xml");
    assert!(
        content.contains(
            r#"office:value-type="string" calcext:value-type="string"><text:p>02134</text:p>"#
        ),
        "{content}"
    );
    assert!(content.contains(r#"office:value-type="float" office:value="3""#));
}

/// A workbook of two sheets, the first with a formula and a zip code kept
/// as text, as flat ODS XML for LibreOffice to turn into .xlsx and .ods.
const ORDERS_FODS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.2" office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
 <office:body><office:spreadsheet>
  <table:table table:name="Orders">
   <table:table-row><table:table-cell office:value-type="string"><text:p>zip</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>qty</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>total</text:p></table:table-cell></table:table-row>
   <table:table-row><table:table-cell office:value-type="string"><text:p>02134</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="3"><text:p>3</text:p></table:table-cell><table:table-cell table:formula="of:=[.B2]*2.5" office:value-type="float" office:value="7.5"><text:p>7.5</text:p></table:table-cell></table:table-row>
  </table:table>
  <table:table table:name="Notes"><table:table-row><table:table-cell office:value-type="string"><text:p>second sheet</text:p></table:table-cell></table:table-row></table:table>
 </office:spreadsheet></office:body>
</office:document>
"#;

/// A workbook writes its first sheet, as values, and says what it left
/// behind: the second sheet and the formula.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_workbook_writes_its_first_sheet_as_values() {
    let dir = tmp();
    let fods = write_bytes(&dir, "orders.fods", ORDERS_FODS.as_bytes());
    for (filter, ext) in [
        ("xlsx:Calc MS Excel 2007 XML", "xlsx"),
        ("ods:calc8", "ods"),
    ] {
        let book = soffice_build(&dir, &fods, filter, ext);
        let (csv, o) = convert_path(&book, "csv");
        let written = std::fs::read_to_string(&csv).unwrap();
        assert_eq!(
            written.lines().collect::<Vec<_>>(),
            ["zip,qty,total", "02134,3,7.5"],
            "{ext}"
        );
        assert_eq!(
            o.warnings,
            [
                "Only the first sheet, Orders, is written; the workbook has 2 and a CSV holds one.",
                "Formulas are written as their values; a CSV holds no formulas.",
            ],
            "{ext}"
        );
    }
}

/// csv -> xlsx -> csv gives back the same lines: no digit lost on the way.
/// Lines, not bytes: LibreOffice ends them with CRLF on Windows, as RFC
/// 4180 has it, and LF elsewhere, and its CSV filter has no option for it.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_csv_survives_a_round_trip_through_xlsx() {
    let dir = tmp();
    let original = "zip,qty,card\n02134,3,4111111111111111\n00501,12,5500000000000004\n";
    let csv = write_bytes(&dir, "orders.csv", original.as_bytes());
    let (xlsx, _) = convert_path(&csv, "xlsx");
    let (back, o) = convert_path(&xlsx, "csv");
    let written = std::fs::read_to_string(&back).unwrap();
    assert_eq!(
        written.lines().collect::<Vec<_>>(),
        original.lines().collect::<Vec<_>>()
    );
    assert!(
        o.warnings.is_empty(),
        "one sheet, no formulas: {:?}",
        o.warnings
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

// --- --upscale --------------------------------------------------------------

/// A flat `w`x`h` PNG, drawn by ffmpeg like the video fixtures.
fn synth_png(dir: &tempfile::TempDir, w: u32, h: u32) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;

    let out = dir.path().join("src.png");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", &format!("testsrc=size={w}x{h}")])
        .args(["-frames:v", "1"])
        .arg(&out)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "building the synthetic image fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

fn upscaled(geometry: &str) -> Tuning {
    Tuning {
        resize: Some(geometry.into()),
        upscale: true,
        ..Default::default()
    }
}

fn warns_of_enlarging(outcome: &exec::Outcome, start: &str) -> bool {
    outcome.notes.iter().any(|n| n.starts_with(start))
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_image_resize_only_enlarges_with_upscale() {
    // ImageMagick enlarges by default: before, this 320x240 came out
    // 1200x900 with nothing said.
    let dir = tmp();
    let src = synth_png(&dir, 320, 240);

    let kept = dir.path().join("kept.jpg");
    let outcome = convert_tuned(
        &src,
        &kept,
        &Tuning {
            resize: Some("1600x900".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(imagemagick_dimensions(&kept), (320, 240));
    assert!(
        !warns_of_enlarging(&outcome, "--resize"),
        "{:?}",
        outcome.notes
    );

    // 1200x900 is 14 times the pixels, past four: asked first.
    let big = dir.path().join("big.jpg");
    let e = convert_tuned(&src, &big, &upscaled("1600x900")).unwrap_err();
    assert_eq!(e.code, convkit_core::ErrorCode::ConfirmationRequired);
    assert!(!big.exists());
    let outcome = convert_tuned_with(&src, &big, &upscaled("1600x900"), true).unwrap();
    assert_eq!(imagemagick_dimensions(&big), (1200, 900));
    assert!(
        warns_of_enlarging(
            &outcome,
            "--resize 1600x900 --upscale enlarges the 320x240 source to 1200x900, about 14 times"
        ),
        "{:?}",
        outcome.notes
    );
    assert_eq!(outcome.enlarged.unwrap().output, Some([1200, 900]));

    // Shrinking, --upscale changes nothing and says nothing.
    let small = dir.path().join("small.jpg");
    let outcome = convert_tuned(&src, &small, &upscaled("160x")).unwrap();
    assert_eq!(imagemagick_dimensions(&small), (160, 120));
    assert_eq!(outcome.enlarged, None);
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn upscale_enlarges_a_video_and_warns() {
    let dir = tmp();
    let src = synth_video(&dir, 640, 360, 30);
    let out = dir.path().join("big.mp4");
    let outcome = convert_tuned(&src, &out, &upscaled("1280x")).unwrap();
    assert_eq!(probe_dims(&out), (1280, 720));
    assert!(
        warns_of_enlarging(
            &outcome,
            "--resize 1280x --upscale enlarges the 640x360 source to 1280x720, about 4 times"
        ),
        "{:?}",
        outcome.notes
    );
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_gif_resize_only_enlarges_with_upscale() {
    // 5 fps keeps the enlarged GIF's palette pass to ten frames.
    let dir = tmp();
    let src = synth_video(&dir, 1280, 720, 5);

    let kept = dir.path().join("kept.gif");
    let outcome = convert_tuned(
        &src,
        &kept,
        &Tuning {
            resize: Some("4000x".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(probe_dims(&kept).0, 1280);
    assert!(
        !warns_of_enlarging(&outcome, "--resize"),
        "{:?}",
        outcome.notes
    );

    // 4000 wide is 9.8 times the pixels, past four: asked first.
    let big = dir.path().join("big.gif");
    let e = convert_tuned(&src, &big, &upscaled("4000x")).unwrap_err();
    assert_eq!(e.code, convkit_core::ErrorCode::ConfirmationRequired);
    let outcome = convert_tuned_with(&src, &big, &upscaled("4000x"), true).unwrap();
    assert_eq!(probe_dims(&big).0, 4000);
    assert!(
        warns_of_enlarging(
            &outcome,
            "--resize 4000x --upscale enlarges the 1280x720 source"
        ),
        "{:?}",
        outcome.notes
    );
}

/// A JPEG whose EXIF says to turn it a quarter: stored 320x240, shown
/// 240x320. ImageMagick writes no EXIF of its own on a fresh image, so the
/// orientation tag is spliced in as a one-entry APP1 segment.
fn synth_jpeg_on_its_side(dir: &tempfile::TempDir) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let plain = dir.path().join("plain.jpg");
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240",
            "-frames:v",
            "1",
        ])
        .arg(&plain)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let jpeg = std::fs::read(&plain).unwrap();
    assert_eq!(&jpeg[..2], b"\xff\xd8", "a JPEG starts with SOI");
    // TIFF header, one IFD entry: Orientation (0x0112), SHORT, 1, value 6.
    let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
    tiff.extend_from_slice(&[
        0x12, 0x01, 0x03, 0x00, 0x01, 0, 0, 0, 0x06, 0, 0, 0, 0, 0, 0, 0,
    ]);
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let len = u16::try_from(payload.len() + 2).unwrap().to_be_bytes();
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1, len[0], len[1]]);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    let rotated = dir.path().join("rotated.jpg");
    std::fs::write(&rotated, out).unwrap();
    rotated
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_upscaled_photo_on_its_side_is_sized_as_it_is_shown() {
    let dir = tmp();
    let src = synth_jpeg_on_its_side(&dir);
    let out = dir.path().join("big.png");
    let outcome = convert_tuned(&src, &out, &upscaled("480x")).unwrap();
    assert_eq!(imagemagick_dimensions(&out), (480, 640));
    let e = outcome.enlarged.expect("an enlargement warns");
    assert!(
        e.warning
            .starts_with("--resize 480x --upscale enlarges the 240x320 source to 480x640"),
        "{}",
        e.warning
    );
    assert!(!e.needs_confirmation, "exactly four times warns only");
}

#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_heic_size_is_read_without_decoding_it() {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let magick = resolver.resolve(Backend::Magick).unwrap().path;
    let first_frame = convkit_core::probe::ImageRead {
        density: None,
        every_input: false,
        every_page: false,
    };
    let p =
        convkit_core::probe::image(&magick, &[fixture("photo.heic")], first_frame, "1x").unwrap();
    assert_eq!(
        p.display_dimensions(),
        Some(imagemagick_dimensions(&fixture("photo.heic")))
    );
}

/// The SVG recipes render at 384 dpi, four times what a size read assumes,
/// so a 100x100 SVG is a 400x400 picture: 300 wide shrinks it, and 800 wide
/// is exactly four times its pixels.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_svg_is_sized_at_the_density_it_renders_at() {
    let dir = tmp();
    let svg = dir.path().join("icon.svg");
    std::fs::write(
        &svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="#c33"/></svg>"##,
    )
    .unwrap();
    let small = dir.path().join("small.png");
    let outcome = convert_tuned(&svg, &small, &upscaled("300x")).unwrap();
    assert_eq!(imagemagick_dimensions(&small), (300, 300));
    assert_eq!(outcome.enlarged, None, "a shrink, not an enlargement");

    let big = dir.path().join("big.png");
    let outcome = convert_tuned(&svg, &big, &upscaled("800x")).unwrap();
    assert_eq!(imagemagick_dimensions(&big), (800, 800));
    let e = outcome.enlarged.expect("an enlargement warns");
    assert_eq!(e.source, Some([400, 400]));
    assert!(!e.needs_confirmation, "exactly four times");
}

/// image -> pdf takes every input: the small one is enlarged 64 times, so
/// it decides the question, and the warning names it.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn several_images_into_one_pdf_are_decided_by_the_one_enlarged_most() {
    let dir = tmp();
    let big = synth_png(&dir, 600, 400);
    let big = {
        let to = dir.path().join("big.png");
        std::fs::rename(&big, &to).unwrap();
        to
    };
    let tiny = synth_png(&dir, 50, 50);
    let tiny = {
        let to = dir.path().join("tiny.png");
        std::fs::rename(&tiny, &to).unwrap();
        to
    };
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    let req = |allow_extreme| exec::Request {
        from: Format::Png,
        to: Format::Pdf,
        inputs: vec![big.clone(), tiny.clone()],
        output: dir.path().join("both.pdf"),
        overwrite: true,
        tuning: upscaled("400x"),
        allow_extreme,
    };
    let e = exec::run(&req(false), &resolver, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, convkit_core::ErrorCode::ConfirmationRequired);
    let outcome = exec::run(&req(true), &resolver, &mut |_| {}).unwrap();
    let w = outcome.enlarged.unwrap().warning;
    assert!(w.contains("enlarges tiny.png (50x50) to 400x400"), "{w}");
}
