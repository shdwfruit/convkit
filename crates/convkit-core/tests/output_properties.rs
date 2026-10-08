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

/// A phone photo gets no frame or alpha note, only the one saying it
/// records where it was taken; a transparent PNG the alpha half; a
/// multi-page TIFF the frame half, plus the alpha half, which a TIFF's
/// header read cannot rule out.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn image_notes_say_only_what_the_source_holds() {
    let said = |o: &exec::Outcome, what: &str| o.warnings.iter().any(|w| w.contains(what));
    let (_, photo) = convert_path(&fixture("photo.heic"), "jpg");
    let expected: &[&str] = if reads_heic_exif() {
        &[
            "The source records a GPS location, and the jpg keeps it; add \
           --strip-metadata to remove it.",
        ]
    } else {
        &[]
    };
    assert_eq!(photo.warnings, expected);

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

// --- Metadata: the location note and --strip-metadata ----------------------

fn stripped() -> Tuning {
    Tuning {
        strip_metadata: true,
        ..Tuning::default()
    }
}

fn magick() -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Magick);
    resolver.resolve(Backend::Magick).unwrap().path
}

/// Runs the resolved ImageMagick (`magick`, or ImageMagick 6's `convert`,
/// which takes the same arguments for everything this file builds).
fn run_magick(args: &[&str]) {
    let out = Command::new(magick())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ImageMagick: {e}"));
    assert!(
        out.status.success(),
        "ImageMagick {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One `identify -format` answer for the first frame.
fn identify_format(path: &Path, format: &str) -> String {
    let first_frame = format!("{}[0]", path.display());
    run_identify(&magick(), &["-format", format, &first_frame])
}

/// Whether this ImageMagick can write `format` (Ubuntu's ImageMagick 6
/// may have no AVIF encoder).
fn magick_writes(format: &str) -> bool {
    let out = Command::new(magick())
        .args(["-list", "format"])
        .output()
        .unwrap_or_else(|e| panic!("failed to run ImageMagick: {e}"));
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let mut it = l.split_whitespace();
        it.next().map(|f| f.trim_end_matches('*')) == Some(format)
            && it.nth(1).is_some_and(|mode| mode.contains('w'))
    })
}

/// One IFD of a big-endian EXIF block placed at offset `at`: entries are
/// `(tag, type, count, value)`, values of four bytes or less inline and the
/// rest after the IFD, padded to an even length. No next IFD.
fn exif_ifd(entries: &[(u16, u16, u32, Vec<u8>)], at: u32) -> Vec<u8> {
    let n = u16::try_from(entries.len()).unwrap();
    let data_at = at + 2 + 12 * u32::from(n) + 4;
    let (mut head, mut data) = (n.to_be_bytes().to_vec(), Vec::new());
    for (tag, kind, count, value) in entries {
        head.extend(tag.to_be_bytes());
        head.extend(kind.to_be_bytes());
        head.extend(count.to_be_bytes());
        if value.len() <= 4 {
            let mut inline = value.clone();
            inline.resize(4, 0);
            head.extend(inline);
        } else {
            head.extend((data_at + u32::try_from(data.len()).unwrap()).to_be_bytes());
            data.extend(value);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    head.extend(0u32.to_be_bytes());
    head.extend(data);
    head
}

/// The EXIF a phone writes, in miniature: IFD0 with the camera, the owner
/// (Artist) and Orientation 6 (stored on its side, as a portrait photo is);
/// an EXIF sub-IFD with the capture time and the body's serial number; and a
/// GPS sub-IFD at 51°30'2.52"N 0°7'28.56"W. Built by hand so no metadata
/// tool is needed; `carries_identifying` looks for these values.
fn gps_exif() -> Vec<u8> {
    let ascii = |tag: u16, s: &str| {
        let bytes = format!("{s}\0").into_bytes();
        (tag, 2u16, u32::try_from(bytes.len()).unwrap(), bytes)
    };
    let rationals = |parts: &[(u32, u32)]| -> Vec<u8> {
        parts
            .iter()
            .flat_map(|(n, d)| n.to_be_bytes().into_iter().chain(d.to_be_bytes()))
            .collect()
    };
    // The sub-IFDs follow IFD0, whose size does not depend on the two
    // pointers' values (inline LONGs), so it is measured with zeros first.
    let ifd0 = |exif_at: u32, gps_at: u32| {
        vec![
            ascii(0x010F, "FakeCam"),
            ascii(0x0110, "FakeCam 9"),
            (0x0112, 3, 1, 6u16.to_be_bytes().to_vec()),
            ascii(0x013B, "Jane Q Owner"),
            (0x8769, 4, 1, exif_at.to_be_bytes().to_vec()),
            (0x8825, 4, 1, gps_at.to_be_bytes().to_vec()),
        ]
    };
    let exif = vec![
        ascii(0x9003, "2026:01:02 03:04:05"),
        ascii(0xA431, "SERIAL-0123456789"),
    ];
    let gps = vec![
        (0x0000, 1, 4, vec![2, 3, 0, 0]),
        ascii(0x0001, "N"),
        (0x0002, 5, 3, rationals(&[(51, 1), (30, 1), (63, 25)])),
        ascii(0x0003, "W"),
        (0x0004, 5, 3, rationals(&[(0, 1), (7, 1), (714, 25)])),
    ];
    let exif_at = 8 + u32::try_from(exif_ifd(&ifd0(0, 0), 8).len()).unwrap();
    let exif_block = exif_ifd(&exif, exif_at);
    let gps_at = exif_at + u32::try_from(exif_block.len()).unwrap();
    let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
    tiff.extend(exif_ifd(&ifd0(exif_at, gps_at), 8));
    tiff.extend(exif_block);
    tiff.extend(exif_ifd(&gps, gps_at));
    tiff
}

/// A phone photo in miniature: stored 96x192 (blue over red) and shown
/// 192x96 with red on the left, because its EXIF says to turn it; the
/// Display P3 colour profile of the committed iPhone photo; and
/// `gps_exif` spliced in as APP1 right after SOI.
fn gps_photo(dir: &tempfile::TempDir) -> PathBuf {
    let icc = dir.path().join("p3.icc");
    let plain = dir.path().join("plain.jpg");
    run_magick(&[
        &fixture("photo.heic").to_string_lossy(),
        &format!("icc:{}", icc.display()),
    ]);
    run_magick(&[
        "-size",
        "96x96",
        "xc:blue",
        "-size",
        "96x96",
        "xc:red",
        "-append",
        "-profile",
        &icc.to_string_lossy(),
        &plain.to_string_lossy(),
    ]);
    let jpeg = std::fs::read(&plain).unwrap();
    assert_eq!(&jpeg[..2], b"\xff\xd8", "a JPEG starts with SOI");
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend(gps_exif());
    let len = u16::try_from(payload.len() + 2).unwrap().to_be_bytes();
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1, len[0], len[1]]);
    out.extend(payload);
    out.extend(&jpeg[2..]);
    let photo = dir.path().join("IMG_0042.jpg");
    std::fs::write(&photo, out).unwrap();
    assert_eq!(identify_format(&photo, "%[orientation]"), "RightTop");
    photo
}

/// Whether the file still holds any of `gps_exif`'s identifying values,
/// in any form ImageMagick writes them: the raw GPS rationals, the text
/// it gives a PNG (ImageMagick 6 spaces them out), the serial number, the
/// owner, or the camera.
fn carries_identifying(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    let needles: [&[u8]; 6] = [
        b"\x00\x00\x00\x33\x00\x00\x00\x01\x00\x00\x00\x1e\x00\x00\x00\x01",
        b"51/1,30/1",
        b"51/1, 30/1",
        b"SERIAL-0123456789",
        b"Jane Q Owner",
        b"FakeCam",
    ];
    needles
        .iter()
        .any(|n| bytes.windows(n.len()).any(|w| w == *n))
}

fn says_location(outcome: &exec::Outcome) -> bool {
    outcome.warnings.iter().any(|w| w.contains("GPS location"))
}

/// For every image target: without the flag the output keeps what the
/// photo carries exactly where the note says it does; with it, nothing
/// identifying is left, the colour profile is still Display P3, and the
/// picture is still upright (shown 192x96, red on the left), which
/// `-auto-orient` running before the strip is what guarantees.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn stripped_images_lose_their_location_and_keep_colour_and_orientation() {
    let dir = tmp();
    let photo = gps_photo(&dir);
    let mut targets = vec!["jpg", "png", "webp", "tiff", "pdf"];
    if magick_writes("AVIF") {
        targets.push("avif");
    } else {
        eprintln!("this ImageMagick cannot write AVIF; the avif target is not checked");
    }
    for ext in targets {
        // jpg -> jpg is no pair without the flag; heic -> jpg's note is
        // checked against the real iPhone photo below.
        if ext != "jpg" {
            let kept = dir.path().join(format!("kept.{ext}"));
            let outcome = convert_tuned(&photo, &kept, &Tuning::default()).unwrap();
            assert_eq!(
                carries_identifying(&kept),
                says_location(&outcome),
                "{ext}: the note must say exactly when the location survives: {:?}",
                outcome.warnings
            );
            assert_eq!(carries_identifying(&kept), ext != "tiff", "{ext}");
        }

        let clean = dir.path().join(format!("clean.{ext}"));
        let outcome = convert_tuned(&photo, &clean, &stripped()).unwrap();
        assert!(!carries_identifying(&clean), "{ext} still identifies");
        assert!(!says_location(&outcome), "{ext}: {:?}", outcome.warnings);
        if ext == "pdf" {
            // Reading a PDF back needs Ghostscript; its colour profile is
            // there as an ICCBased colour space.
            let bytes = std::fs::read(&clean).unwrap();
            assert!(bytes.windows(8).any(|w| w == b"ICCBased"), "pdf");
            continue;
        }
        assert_eq!(
            identify_format(&clean, "%[icc:description]"),
            "Display P3",
            "{ext}"
        );
        assert_eq!(
            imagemagick_dimensions(&clean),
            (192, 96),
            "{ext} is on its side"
        );
        let red_left = identify_format(&clean, "%[fx:p{10,48}.r > 0.8 && p{10,48}.b < 0.2]");
        assert_eq!(red_left.trim(), "1", "{ext} is turned the wrong way");
    }
}

/// The committed iPhone photo carries real GPS coordinates; stripped, the
/// jpg has none, and keeps its size and colour profile.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_real_iphone_photo_loses_its_gps() {
    let dir = tmp();
    let src = fixture("photo.heic");
    // The photo's GPS latitude as its big-endian EXIF stores it: 35/1, 43/1.
    let latitude = b"\x00\x00\x00\x23\x00\x00\x00\x01\x00\x00\x00\x2b\x00\x00\x00\x01";
    let has_gps = |p: &Path| {
        std::fs::read(p)
            .unwrap()
            .windows(latitude.len())
            .any(|w| w == latitude)
    };
    let kept = dir.path().join("kept.jpg");
    let outcome = convert_tuned(&src, &kept, &Tuning::default()).unwrap();
    assert!(has_gps(&kept));
    if reads_heic_exif() {
        assert!(says_location(&outcome), "{:?}", outcome.warnings);
    } else {
        eprintln!("this ImageMagick cannot read a HEIC's EXIF; the note is not checked");
    }

    let clean = dir.path().join("clean.jpg");
    convert_tuned(&src, &clean, &stripped()).unwrap();
    assert!(!has_gps(&clean));
    assert_eq!(identify_format(&clean, "%[icc:description]"), "Display P3");
    assert_eq!(imagemagick_dimensions(&clean), (4032, 3024));
}

/// Whether this ImageMagick can read the EXIF of a HEIC photo, which the
/// location note depends on. ImageMagick 6 (what Debian and Ubuntu ship)
/// stores a HEIC's EXIF without the `Exif\0\0` marker its own EXIF parser
/// looks for, so it reads none: the profile is still attached, written out
/// and removed by `--strip-metadata`, but the note cannot see it.
fn reads_heic_exif() -> bool {
    identify_format(&fixture("photo.heic"), "[%[EXIF:GPSLatitude]]") != "[]"
}

/// Runs the resolved ffmpeg, quietly, failing the test with its stderr.
fn run_ffmpeg(args: &[&str]) {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let out = Command::new(resolver.resolve(Backend::Ffmpeg).unwrap().path)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffmpeg: {e}"));
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Two phone-style clips, both turned a quarter by a display matrix, as a
/// portrait phone video is: `apple.mov` with Apple's mdta location key, as
/// iPhones write it, and `classic.mp4` with the classic `location` tag
/// (written as `loci`), a title and an artist. The tags and the rotation
/// are added by a stream copy, which carries the matrix as it is.
fn located_clips(dir: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let plain = dir.path().join("plain.mp4");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=64x48:rate=10:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=duration=2",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-shortest",
        &plain.to_string_lossy(),
    ]);
    let apple = dir.path().join("apple.mov");
    run_ffmpeg(&[
        "-display_rotation",
        "90",
        "-i",
        &plain.to_string_lossy(),
        "-c",
        "copy",
        "-movflags",
        "use_metadata_tags",
        "-metadata",
        "com.apple.quicktime.location.ISO6709=+51.5007-000.1246+010.000/",
        "-metadata",
        "com.apple.quicktime.make=Apple",
        &apple.to_string_lossy(),
    ]);
    let classic = dir.path().join("classic.mp4");
    run_ffmpeg(&[
        "-display_rotation",
        "90",
        "-i",
        &plain.to_string_lossy(),
        "-c",
        "copy",
        "-metadata",
        "location=+51.5007-000.1246/",
        "-metadata",
        "title=Clip",
        "-metadata",
        "artist=Band",
        &classic.to_string_lossy(),
    ]);
    (apple, classic)
}

/// Whether a video or audio file still records the fixtures' location: as
/// ISO 6709 text (mdta keys, `©xyz`, Matroska and ID3 tags) or as the
/// binary `loci` box an mp4 writes.
fn carries_location(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    let needles: [&[u8]; 3] = [b"+51.5007", b"loci", b"\xa9xyz"];
    needles
        .iter()
        .any(|n| bytes.windows(n.len()).any(|w| w == *n))
}

/// The container's tags, as ffprobe reads them.
fn format_tags(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffprobe);
    let out = Command::new(resolver.resolve(Backend::Ffprobe).unwrap().path)
        .args(["-v", "quiet", "-print_format", "json", "-show_format"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run ffprobe: {e}"));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["format"]["tags"].as_object().cloned().unwrap_or_default()
}

fn tag<'a>(tags: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
}

/// Each clip into each video container: without the flag the location
/// survives exactly where the note says (Apple's key is dropped by mp4,
/// kept by mkv and webm); with it, none is left, the title and artist are,
/// a copy is still a copy, and a copied clip is still turned.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn stripped_video_loses_its_location_and_keeps_title_rotation_and_the_copy() {
    let dir = tmp();
    let (apple, classic) = located_clips(&dir);
    for (src, ext) in [
        (&apple, "mp4"),
        (&apple, "mkv"),
        (&apple, "webm"),
        (&classic, "mov"),
        (&classic, "mkv"),
        (&classic, "webm"),
    ] {
        let from = src.file_name().unwrap().to_string_lossy();
        let name = format!("{from} -> {ext}");
        let kept = dir.path().join(format!("kept-{from}.{ext}"));
        let outcome = convert_tuned(src, &kept, &Tuning::default()).unwrap();
        assert_eq!(
            carries_location(&kept),
            says_location(&outcome),
            "{name}: the note must say exactly when the location survives: {:?}",
            outcome.warnings
        );
        assert_eq!(
            carries_location(&kept),
            !(src == &apple && ext == "mp4"),
            "{name}"
        );

        let clean = dir.path().join(format!("clean-{from}.{ext}"));
        let outcome = convert_tuned(src, &clean, &stripped()).unwrap();
        assert!(!carries_location(&clean), "{name} still records it");
        assert!(!says_location(&outcome), "{name}: {:?}", outcome.warnings);
        if src == &classic {
            let tags = format_tags(&clean);
            assert_eq!(tag(&tags, "title"), Some("Clip"), "{name}: {tags:?}");
            assert_eq!(tag(&tags, "artist"), Some("Band"), "{name}: {tags:?}");
        }
        if ext != "webm" {
            assert!(outcome.remuxed, "{name} was re-encoded");
            assert_eq!(
                probe_media(&clean).rotation.map(i32::abs),
                Some(90),
                "{name}"
            );
        }
    }
    // The re-encode path clears them too.
    let crf = dir.path().join("crf.mov");
    let tuning = Tuning {
        crf: Some(30),
        ..stripped()
    };
    convert_tuned(&classic, &crf, &tuning).unwrap();
    assert!(!carries_location(&crf));
    assert_eq!(tag(&format_tags(&crf), "title"), Some("Clip"));
}

/// A clip stripped into its own container is a stream copy of every
/// stream, rotation included, with the location gone.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_video_stripped_in_place_is_copied_not_re_encoded() {
    let dir = tmp();
    let (apple, _) = located_clips(&dir);
    let clean = dir.path().join("apple-stripped.mov");
    convert_tuned(&apple, &clean, &stripped()).unwrap();
    assert!(!carries_location(&clean));
    let (before, after) = (probe_media(&apple), probe_media(&clean));
    assert_eq!(after.video_codec, before.video_codec);
    assert_eq!(after.audio_codecs, before.audio_codecs);
    assert_eq!(after.rotation.map(i32::abs), Some(90));
}

/// A compressed tiff stripped into a tiff stays compressed, losslessly;
/// an uncompressed one stays uncompressed.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn a_tiff_stripped_in_place_keeps_its_compression() {
    let dir = tmp();
    for (compression, expected) in [("LZW", "Zip"), ("None", "None")] {
        let src = dir.path().join(format!("scan-{compression}.tiff"));
        run_magick(&[
            "-size",
            "256x256",
            "gradient:red-blue",
            "-compress",
            compression,
            &src.to_string_lossy(),
        ]);
        let out = dir.path().join(format!("scan-{compression}-stripped.tiff"));
        convert_tuned(&src, &out, &stripped()).unwrap();
        assert_eq!(identify_format(&out, "%C"), expected, "{compression}");
        // `%#` hashes the pixels alone, so the same picture stored another
        // way hashes the same.
        assert_eq!(
            identify_format(&out, "%#"),
            identify_format(&src, "%#"),
            "{compression}: the pixels changed"
        );
    }
}

/// An mkv carrying a font attachment, as subtitled anime does: Matroska
/// refuses an attachment whose tags were cleared, so the strip has to give
/// them back. The font stays, the location goes.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn an_mkv_with_a_font_attachment_can_be_stripped() {
    let dir = tmp();
    let font = dir.path().join("font.ttf");
    std::fs::write(&font, b"not really a font").unwrap();
    let anime = dir.path().join("anime.mkv");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=64x48:rate=10:duration=1",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-attach",
        &font.to_string_lossy(),
        "-metadata:s:t",
        "mimetype=application/x-truetype-font",
        "-metadata",
        "location=+51.5007-000.1246/",
        &anime.to_string_lossy(),
    ]);
    let clean = dir.path().join("anime-stripped.mkv");
    convert_tuned(&anime, &clean, &stripped()).unwrap();
    assert!(!carries_location(&clean));
    assert_eq!(probe_media(&clean).attachment_streams, 1);
}

/// An m4a with a title, an artist and a location, into mp3: the note
/// without the flag, and with it the location gone and the two tags kept.
#[test]
#[ignore = "requires backends; run with --ignored"]
fn stripped_audio_keeps_its_title_and_artist() {
    let dir = tmp();
    let song = dir.path().join("song.m4a");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=duration=1",
        "-c:a",
        "aac",
        "-metadata",
        "title=Song",
        "-metadata",
        "artist=Band",
        "-metadata",
        "location=+51.5007-000.1246/",
        &song.to_string_lossy(),
    ]);
    let kept = dir.path().join("kept.mp3");
    let outcome = convert_tuned(&song, &kept, &Tuning::default()).unwrap();
    assert!(carries_location(&kept));
    assert!(says_location(&outcome), "{:?}", outcome.warnings);

    let clean = dir.path().join("clean.mp3");
    let outcome = convert_tuned(&song, &clean, &stripped()).unwrap();
    assert!(!carries_location(&clean));
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    let tags = format_tags(&clean);
    assert_eq!(tag(&tags, "title"), Some("Song"), "{tags:?}");
    assert_eq!(tag(&tags, "artist"), Some("Band"), "{tags:?}");
}

// --- Cutting a range (--start / --end / --duration) -----------------------

/// A `secs`-second h264+aac clip with a keyframe every 5 s exactly, so a
/// stream copy that started anywhere but a keyframe would show it.
fn synth_cuttable(
    dir: &tempfile::TempDir,
    name: &str,
    rate: &str,
    secs: u32,
    extra: &[&str],
) -> PathBuf {
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let out = dir.path().join(name);
    let gop = "150";
    let result = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size=320x180:rate={rate}"),
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", &secs.to_string()])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-g",
            gop,
            "-keyint_min",
            gop,
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
        ])
        .args(extra)
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    out
}

fn ranged(start: Option<&str>, end: Option<&str>) -> Tuning {
    Tuning {
        range: convkit_core::trim::Range::new(
            start.map(|s| convkit_core::trim::parse_time(s).unwrap()),
            end.map(|s| convkit_core::trim::parse_time(s).unwrap()),
            None,
        )
        .unwrap(),
        ..Tuning::default()
    }
}

/// Frames actually decoded, which an edit list cannot hide.
fn decoded_frames(path: &Path) -> u64 {
    let ffprobe = Resolver::new().resolve(Backend::Ffprobe).unwrap().path;
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn duration_ms(path: &Path) -> u64 {
    probe_media(path).duration_ms.unwrap()
}

#[test]
#[ignore]
fn a_cut_after_zero_is_frame_exact_in_every_video_container() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    for ext in ["mp4", "mkv", "mov", "webm"] {
        let out = dir.path().join(format!("cut.{ext}"));
        convert_tuned(&src, &out, &ranged(Some("7"), Some("10"))).unwrap();
        assert_eq!(
            decoded_frames(&out),
            90,
            "{ext}: 3 s at 30 fps, nothing from the keyframe at 5 s"
        );
    }
}

/// An `ffprobe -show_entries` value, read with edit lists ignored when
/// asked, so audio an mp4 only hides is still counted.
fn probe_value(path: &Path, entries: &str, select: &str, ignore_editlist: bool) -> f64 {
    let ffprobe = Resolver::new().resolve(Backend::Ffprobe).unwrap().path;
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "error"]);
    if ignore_editlist {
        cmd.args(["-ignore_editlist", "1"]);
    }
    let out = cmd
        .args(["-select_streams", select, "-show_entries", entries])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("{} {entries}: {text:?}: {e}", path.display()))
}

/// An input `-ss` seeks the video; copied audio must still start at the
/// cut, not at the keyframe 2 s before it.
#[test]
#[ignore]
fn a_cut_with_copied_audio_starts_the_sound_at_the_cut() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let mkv = dir.path().join("cut.mkv");
    convert_tuned(&src, &mkv, &ranged(Some("7"), Some("10"))).unwrap();
    // Matroska has no edit lists: early sound pushes the picture later
    // and makes the file longer than the cut.
    let video_start = probe_value(&mkv, "stream=start_time", "v:0", false);
    assert!(video_start < 0.1, "mkv picture starts at {video_start} s");
    let length = probe_media(&mkv).duration_ms.unwrap();
    assert!(length < 3_100, "a 3 s cut is {length} ms long");
    let m4a = dir.path().join("cut.m4a");
    convert_tuned(&src, &m4a, &ranged(Some("7"), Some("10"))).unwrap();
    let held = probe_value(&m4a, "stream=duration", "a:0", true);
    assert!(held < 3.1, "m4a holds {held} s of audio, edit list aside");
}

/// A webm written live, as a browser's MediaRecorder writes one, carries no
/// duration. A range from the start needs none, so it is cut all the same.
#[test]
#[ignore]
fn a_recording_without_a_duration_is_still_cut() {
    let dir = tmp();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let src = dir.path().join("live.webm");
    let r = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=30:duration=6",
        ])
        .args([
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
        ])
        .args(["-live", "1"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(
        probe_media(&src).duration_ms,
        None,
        "the fixture must lack one"
    );
    let out = dir.path().join("cut.mkv");
    let o = convert_tuned(&src, &out, &ranged(Some("1"), Some("3"))).unwrap();
    assert_eq!(decoded_frames(&out), 60, "2 s at 30 fps");
    assert!(
        o.warnings
            .iter()
            .any(|w| w.contains("Source length could not be determined")),
        "{:?}",
        o.warnings
    );
}

/// A range covering the whole file cuts nothing, so a file already under
/// `--max-size` is copied as it is, as it would be with no range at all.
#[test]
#[ignore]
fn a_whole_file_range_under_max_size_copies_a_file_that_already_fits() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let out = dir.path().join("src-0s-30s.mp4");
    let mut t = ranged(None, Some("30"));
    t.max_size = Some(convkit_core::size::parse("100mb").unwrap());
    let o = convert_tuned(&src, &out, &t).unwrap();
    assert_eq!(o.bytes, std::fs::metadata(&src).unwrap().len());
    assert!(
        o.warnings.iter().any(|w| w.contains("nothing was cut")),
        "{:?}",
        o.warnings
    );
}

#[test]
#[ignore]
fn a_cut_from_zero_is_a_stream_copy() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let out = dir.path().join("cut.mkv");
    let o = convert_tuned(&src, &out, &ranged(None, Some("10"))).unwrap();
    assert!(o.remuxed, "{:?}", o.warnings);
    let d = duration_ms(&out);
    assert!((9_900..=10_200).contains(&d), "{d}");
    assert_eq!(decoded_frames(&out), 300);
}

/// With B-frames, a copy that stops at the end carried two frames past it,
/// so a cut from 0 with an end re-encodes, and is exact.
#[test]
#[ignore]
fn a_cut_from_zero_of_video_with_b_frames_ends_exactly() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 4, &["-bf", "3"]);
    assert!(probe_media(&src).video_reorders, "the source has B-frames");
    for ext in ["mp4", "mkv"] {
        let out = dir.path().join(format!("cut.{ext}"));
        let o = convert_tuned(&src, &out, &ranged(None, Some("1.5"))).unwrap();
        assert!(!o.remuxed, "{ext}");
        assert_eq!(decoded_frames(&out), 45, "{ext}: 1.5 s at 30 fps");
    }
}

#[test]
#[ignore]
fn a_trim_into_the_same_format_works() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let out = dir.path().join("src-7s-10s.mp4");
    convert_tuned(&src, &out, &ranged(Some("7"), Some("10"))).unwrap();
    assert_eq!(decoded_frames(&out), 90);
}

#[test]
#[ignore]
fn a_cut_to_gif_and_to_audio_is_the_length_asked_for() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let gif = dir.path().join("cut.gif");
    convert_tuned(&src, &gif, &ranged(Some("7"), Some("10"))).unwrap();
    assert_eq!(decoded_frames(&gif), 45, "3 s at the GIF default of 15 fps");
    for ext in ["mp3", "m4a", "wav", "flac"] {
        let out = dir.path().join(format!("cut.{ext}"));
        convert_tuned(&src, &out, &ranged(Some("7"), Some("10"))).unwrap();
        let d = duration_ms(&out);
        assert!((2_950..=3_100).contains(&d), "{ext}: {d} ms");
    }
}

#[test]
#[ignore]
fn a_2997_cut_lands_within_a_frame() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mov", "30000/1001", 20, &[]);
    let out = dir.path().join("cut.mp4");
    convert_tuned(&src, &out, &ranged(Some("6.5"), Some("9.5"))).unwrap();
    let frames = decoded_frames(&out);
    assert!(
        (89..=91).contains(&frames),
        "3 s at 29.97 fps is ~90 frames, got {frames}"
    );
}

/// A file whose timestamps start at 3 s: `--start 8` is 8 s into what a
/// player shows, leaving 2 s, not 8 s of absolute time (which would leave
/// 5). Written to mkv, whose variable-rate output keeps the frames as they
/// are; ffmpeg 6.1's constant-rate mp4 output adds a frame to any mkv
/// source's millisecond timestamps, cut or not.
#[test]
#[ignore]
fn a_cut_of_an_offset_source_is_relative_to_what_a_player_shows() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mkv", "30", 10, &["-output_ts_offset", "3"]);
    let out = dir.path().join("cut.mkv");
    convert_tuned(&src, &out, &ranged(Some("8"), None)).unwrap();
    assert_eq!(decoded_frames(&out), 60, "the last 2 s, at 30 fps");
}

#[test]
#[ignore]
fn a_cut_mp3_keeps_its_cover_art() {
    let dir = tmp();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let art = synth_png(&dir, 64, 64);
    let src = dir.path().join("song.mp3");
    let r = Command::new(&ffmpeg)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20",
            "-i",
        ])
        .arg(&art)
        .args([
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "png",
            "-disposition:v",
            "attached_pic",
        ])
        .arg(&src)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let out = dir.path().join("song-5s-10s.mp3");
    convert_tuned(&src, &out, &ranged(Some("5"), Some("10"))).unwrap();
    let streams = probe_streams_json(&resolver.resolve(Backend::Ffprobe).unwrap().path, &out);
    assert!(
        streams.iter().any(|s| s["codec_type"] == "video"),
        "cover art lost: {streams:?}"
    );
}

/// conv trim's picture: a frame read over a pipe, as raw RGB or a PNG.
#[test]
#[ignore]
fn a_trim_frame_is_grabbed_over_a_pipe() {
    use convkit_core::frames::{grab, Pixels, Want};
    let dir = tmp();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let src = dir.path().join("red.mp4");
    let r = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=320x180:rate=30:duration=3",
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let Pixels::Rgb { data, .. } = grab(
        &ffmpeg,
        &src,
        1_500,
        Want::Rgb {
            width: 8,
            height: 6,
        },
    )
    .unwrap() else {
        panic!("asked for rgb")
    };
    assert_eq!(data.len(), 8 * 6 * 3);
    for px in data.chunks(3) {
        assert!(px[0] > 200 && px[1] < 40 && px[2] < 40, "{px:?}");
    }
    let Pixels::Png(png) = grab(&ffmpeg, &src, 0, Want::Png { width: 64 }).unwrap() else {
        panic!("asked for png")
    };
    assert!(png.starts_with(b"\x89PNG"));
    assert!(
        grab(&ffmpeg, &src, 60_000, Want::Png { width: 64 }).is_err(),
        "past the end there is no frame"
    );
}

/// conv trim's loudness bar: 1 s of tone, then 1 s of silence.
#[test]
#[ignore]
fn a_trim_loudness_tells_sound_from_silence() {
    use convkit_core::frames::{dbfs, loudness};
    let dir = tmp();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let src = dir.path().join("tone.m4a");
    let r = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
        .args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono:d=1"])
        .args(["-filter_complex", "[0][1]concat=n=2:v=0:a=1", "-c:a", "aac"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let mut levels = Vec::new();
    loudness(&ffmpeg, &src, &mut |batch| levels.extend_from_slice(batch)).unwrap();
    assert!(
        (195..=210).contains(&levels.len()),
        "{} buckets",
        levels.len()
    );
    // lavfi's sine is an eighth of full scale: about -21 dBFS.
    let loud = levels[10..90].iter().all(|&l| dbfs(l) > -25.0);
    let quiet = levels[110..190].iter().all(|&l| dbfs(l) < -60.0);
    assert!(loud && quiet, "{levels:?}");
}

/// conv trim's video-only clips: no audio track in any video container,
/// whichever path builds the command (the webm one is the static recipe,
/// whose audio filter must not trip over the missing audio).
#[test]
#[ignore]
fn a_silent_cut_has_no_audio_track() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let resolver = Resolver::new();
    let ffprobe = resolver.resolve(Backend::Ffprobe).unwrap().path;
    for ext in ["mp4", "mkv", "mov", "webm"] {
        let out = dir.path().join(format!("silent.{ext}"));
        let mut t = ranged(Some("7"), Some("10"));
        t.mute = true;
        convert_tuned(&src, &out, &t).unwrap_or_else(|e| panic!("{ext}: {e}"));
        let streams = probe_streams_json(&ffprobe, &out);
        assert!(
            streams.iter().all(|s| s["codec_type"] != "audio"),
            "{ext}: {streams:?}"
        );
        assert_eq!(decoded_frames(&out), 90, "{ext}");
    }
}

#[test]
#[ignore]
fn a_wav_cut_into_itself_keeps_its_bit_depth() {
    let dir = tmp();
    let resolver = Resolver::new();
    require_backend(&resolver, Backend::Ffmpeg);
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).unwrap().path;
    let src = dir.path().join("take.wav");
    let r = Command::new(&ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=96000:duration=10",
        ])
        .args(["-c:a", "pcm_s24le"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let out = dir.path().join("take-2s-5s.wav");
    convert_tuned(&src, &out, &ranged(Some("2"), Some("5"))).unwrap();
    let streams = probe_streams_json(&resolver.resolve(Backend::Ffprobe).unwrap().path, &out);
    assert_eq!(streams[0]["codec_name"], "pcm_s24le", "{streams:?}");
    let d = duration_ms(&out);
    assert!((2_990..=3_010).contains(&d), "{d}");
}

/// A copied FLAC cut kept the source's header and said it was as long as
/// the whole file.
#[test]
#[ignore]
fn a_flac_cut_into_itself_says_its_own_length_and_keeps_its_bit_depth() {
    let dir = tmp();
    let src = dir.path().join("take.flac");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=96000:duration=10",
        "-c:a",
        "flac",
        "-sample_fmt",
        "s32",
        "-bits_per_raw_sample",
        "24",
        src.to_str().unwrap(),
    ]);
    let out = dir.path().join("take-2s-5s.flac");
    convert_tuned(&src, &out, &ranged(Some("2"), Some("5"))).unwrap();
    let ffprobe = Resolver::new().resolve(Backend::Ffprobe).unwrap().path;
    let streams = probe_streams_json(&ffprobe, &out);
    assert_eq!(streams[0]["bits_per_raw_sample"], "24", "{streams:?}");
    assert_eq!(
        streams[0]["duration_ts"], 288_000,
        "3 s at 96 kHz: {streams:?}"
    );
    let d = duration_ms(&out);
    assert!((2_990..=3_010).contains(&d), "{d}");
}

#[test]
#[ignore]
fn a_sized_cut_is_sized_for_the_clip() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 20, &[]);
    let out = dir.path().join("cut.mp4");
    let mut t = ranged(Some("5"), Some("15"));
    t.max_size = Some(convkit_core::size::parse("300kb").unwrap());
    let o = convert_tuned(&src, &out, &t).unwrap();
    assert!(o.bytes <= 300_000, "{}", o.bytes);
    let d = duration_ms(&out);
    assert!((9_900..=10_100).contains(&d), "{d}");
}

#[test]
#[ignore]
fn the_gif_memory_note_follows_the_cut() {
    let dir = tmp();
    let src = synth_cuttable(&dir, "src.mp4", "30", 40, &[]);
    let o = convert_tuned(
        &src,
        &dir.path().join("cut.gif"),
        &ranged(Some("0:05"), Some("0:10")),
    )
    .unwrap();
    assert!(
        o.warnings.iter().all(|w| !w.contains("buffered in memory")),
        "{:?}",
        o.warnings
    );
}

/// A clip cut and stripped into its own format in one run loses the
/// location and keeps the title and artist, whether the cut is copied
/// (from the start) or re-encoded (from later on).
#[test]
#[ignore]
fn a_clip_cut_and_stripped_into_its_own_format_loses_its_location() {
    let dir = tmp();
    let (_, classic) = located_clips(&dir);
    for start in [None, Some("0.5")] {
        let tuning = Tuning {
            strip_metadata: true,
            ..ranged(start, Some("1.5"))
        };
        let out = dir
            .path()
            .join(format!("clip-{}.mp4", start.unwrap_or("0")));
        convert_tuned(&classic, &out, &tuning).unwrap();
        assert!(!carries_location(&out), "{start:?} still records it");
        let tags = format_tags(&out);
        assert_eq!(tag(&tags, "title"), Some("Clip"), "{start:?}: {tags:?}");
        assert_eq!(tag(&tags, "artist"), Some("Band"), "{start:?}: {tags:?}");
    }
}

/// How many lines ffprobe prints for these arguments: one a chapter, or
/// one a packet.
fn ffprobe_lines(path: &Path, args: &[&str]) -> usize {
    let ffprobe = Resolver::new().resolve(Backend::Ffprobe).unwrap().path;
    let out = Command::new(ffprobe)
        .args(["-v", "error"])
        .args(args)
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

fn chapters(path: &Path) -> usize {
    ffprobe_lines(path, &["-show_chapters"])
}

/// A subtitle cue that starts before the cut is left out, rather than kept
/// before it with the picture and sound pushed back to make room, and a
/// clip lists none of the source's chapters, which ffmpeg moves back by the
/// start but never ends at the clip's end.
#[test]
#[ignore]
fn a_cut_mkv_with_subtitles_and_chapters_starts_at_the_cut() {
    let dir = tmp();
    let plain = synth_cuttable(&dir, "plain.mkv", "30", 20, &[]);
    let srt = dir.path().join("subs.srt");
    std::fs::write(
        &srt,
        "1\n00:00:05,000 --> 00:00:08,000\nacross the cut\n\n\
         2\n00:00:08,500 --> 00:00:09,000\ninside it\n",
    )
    .unwrap();
    let meta = dir.path().join("chapters.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n",
    )
    .unwrap();
    let src = dir.path().join("src.mkv");
    run_ffmpeg(&[
        "-i",
        plain.to_str().unwrap(),
        "-i",
        srt.to_str().unwrap(),
        "-i",
        meta.to_str().unwrap(),
        "-map",
        "0",
        "-map",
        "1",
        "-map_chapters",
        "2",
        "-c",
        "copy",
        src.to_str().unwrap(),
    ]);
    assert_eq!(chapters(&src), 2);

    let mkv = dir.path().join("cut.mkv");
    convert_tuned(&src, &mkv, &ranged(Some("7"), Some("10"))).unwrap();
    let video_start = probe_value(&mkv, "stream=start_time", "v:0", false);
    assert!(video_start < 0.1, "the picture starts at {video_start} s");
    let cues = ffprobe_lines(
        &mkv,
        &["-select_streams", "s:0", "-show_entries", "packet=pts_time"],
    );
    assert_eq!(cues, 1, "only the cue inside the cut");
    assert_eq!(decoded_frames(&mkv), 90);
    assert_eq!(chapters(&mkv), 0);

    let m4a = dir.path().join("cut.m4a");
    convert_tuned(&src, &m4a, &ranged(Some("7"), Some("10"))).unwrap();
    assert_eq!(chapters(&m4a), 0);
}
