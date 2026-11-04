//! `conv merge` of PDFs with pictures, and of video and audio, against the
//! real backends.
//!
//! `#[ignore]`-gated like `output_properties.rs`: `cargo test` stays green
//! with nothing installed, and `cargo test -- --ignored` runs these
//! wherever ffmpeg, ffprobe, qpdf, Typst and (for HEIC) ImageMagick
//! resolve.
//!
//! The clips are made here with ffmpeg: two seconds of black with one
//! white frame at 1.0 s, and a beep from 1.0 s. After a join, every
//! clip's flash must still have its beep right after it, which is what
//! keeps the sound in step with the picture at each join.

use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::merge::{clips, pages};
use convkit_core::{Backend, Format, ResolvedBackend, Resolver};

fn backend(b: Backend) -> ResolvedBackend {
    Resolver::new().resolve(b).unwrap_or_else(|e| {
        let hint = e
            .remediation
            .as_ref()
            .and_then(|r| r.managed.as_deref().or(r.manual.as_deref()))
            .unwrap_or("no remediation available");
        panic!("backend_missing: {} not found -- {hint}", b.exe_name())
    })
}

fn fixture(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    assert!(p.is_file(), "missing tests/fixtures/{name}");
    p
}

fn run(program: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success() || out.status.code() == Some(3),
        "{} {args:?} failed: {}",
        program.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ffmpeg(args: &[&str]) {
    run(&backend(Backend::Ffmpeg).path, args);
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// What a clip is made of, for `clip`.
struct Spec<'a> {
    size: (u32, u32),
    fps: u32,
    /// Sample rate and channels; `None` for no sound.
    sound: Option<(u32, u32)>,
    video: &'a str,
    audio: &'a str,
}

const PLAIN: Spec<'static> = Spec {
    size: (320, 180),
    fps: 30,
    sound: Some((48_000, 2)),
    video: "libx264",
    audio: "aac",
};

/// A two-second clip: black with a white frame at 1.0 s and a 1 kHz beep
/// from 1.0 s to 1.1 s.
fn clip(dir: &Path, name: &str, spec: &Spec) -> PathBuf {
    let out = dir.join(name);
    let (w, h) = spec.size;
    let fps = spec.fps;
    let video = format!(
        "color=black:s={w}x{h}:r={fps}:d=2,drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:\
         enable='between(t,1,1+1/{fps}-0.0001)',format=yuv420p"
    );
    let mut args: Vec<String> = vec!["-v", "error", "-y", "-f", "lavfi", "-i", &video]
        .into_iter()
        .map(String::from)
        .collect();
    if let Some((rate, channels)) = spec.sound {
        let layout = if channels == 1 { "mono" } else { "stereo" };
        args.extend([
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!(
                "sine=f=1000:r={rate}:d=2,volume=enable='not(between(t,1,1.1))':volume=0,\
                     aformat=channel_layouts={layout}"
            ),
            "-c:a".to_string(),
            spec.audio.to_string(),
        ]);
    }
    args.extend(["-c:v", spec.video, "-g", "30"].map(String::from));
    match spec.video {
        "libx265" => {
            args.extend(["-tag:v", "hvc1", "-x265-params", "log-level=error"].map(String::from))
        }
        "libvpx-vp9" => args.extend(
            [
                "-b:v",
                "0",
                "-crf",
                "40",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
            ]
            .map(String::from),
        ),
        _ => {}
    }
    args.push(s(&out).to_string());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    ffmpeg(&argv);
    out
}

/// `input` with its display matrix turned 90 degrees, as a phone filming
/// upright stores it.
fn turned(dir: &Path, input: &Path, name: &str) -> PathBuf {
    let out = dir.join(name);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-display_rotation",
        "90",
        "-i",
        s(input),
        "-c",
        "copy",
        s(&out),
    ]);
    out
}

/// `input` with every seventh frame dropped and a 600 time base, as a phone
/// records in low light: a variable frame rate whose `r_frame_rate` says 30
/// and whose average is lower.
fn variable_rate(dir: &Path, input: &Path, name: &str) -> PathBuf {
    let out = dir.join(name);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-i",
        s(input),
        "-vf",
        "select='mod(n\\,7)'",
        "-fps_mode",
        "vfr",
        "-c:v",
        "libx264",
        "-c:a",
        "copy",
        "-video_track_timescale",
        "600",
        s(&out),
    ]);
    out
}

fn probe(p: &Path) -> serde_json::Value {
    let (out, _) = run(
        &backend(Backend::Ffprobe).path,
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            s(p),
        ],
    );
    serde_json::from_str(&out).unwrap()
}

fn duration(v: &serde_json::Value) -> f64 {
    v["format"]["duration"].as_str().unwrap().parse().unwrap()
}

fn stream<'a>(v: &'a serde_json::Value, kind: &str) -> &'a serde_json::Value {
    v["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["codec_type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} stream"))
}

/// When each white frame starts, in seconds.
fn flashes(p: &Path) -> Vec<f64> {
    let (out, _) = run(
        &backend(Backend::Ffmpeg).path,
        &[
            "-v",
            "error",
            "-i",
            s(p),
            "-vf",
            "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
            "-f",
            "null",
            "-",
        ],
    );
    let mut times = Vec::new();
    let (mut at, mut lit) = (0.0, false);
    for line in out.lines() {
        if let Some(t) = line.split("pts_time:").nth(1) {
            at = t.trim().parse().unwrap();
        } else if let Some(v) = line.strip_prefix("lavfi.signalstats.YAVG=") {
            // A padded clip's flash fills only part of the frame.
            let bright = v.trim().parse::<f64>().unwrap() > 40.0;
            if bright && !lit {
                times.push(at);
            }
            lit = bright;
        }
    }
    times
}

/// When each beep starts, in seconds.
fn beeps(p: &Path) -> Vec<f64> {
    let (_, err) = run(
        &backend(Backend::Ffmpeg).path,
        &[
            "-v",
            "info",
            "-i",
            s(p),
            "-map",
            "0:a:0",
            "-af",
            "silencedetect=noise=-30dB:d=0.05",
            "-f",
            "null",
            "-",
        ],
    );
    let total = duration(&probe(p));
    err.lines()
        .filter_map(|l| l.split("silence_end: ").nth(1))
        .filter_map(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        // The silence that runs to the end of the file is not a beep.
        .filter(|t| total - t > 0.05)
        .collect()
}

/// Every flash has its beep within 30 ms after it: the beep is made from
/// 1.0 s like the flash, and starts on the first audio frame at or after
/// it, 3 ms late at 48 kHz and 22 ms at 44.1 kHz in the clips made here.
/// A picture one frame (33 ms) early or late against its sound fails.
fn assert_in_step(p: &Path, expected_flashes: &[f64], expected_beeps: usize) {
    let f = flashes(p);
    let b = beeps(p);
    assert_eq!(f.len(), expected_flashes.len(), "flashes {f:?}");
    for (got, want) in f.iter().zip(expected_flashes) {
        assert!(
            (got - want).abs() < 0.04,
            "flash at {got}, wanted {want}: {f:?}"
        );
    }
    assert_eq!(b.len(), expected_beeps, "beeps {b:?}, flashes {f:?}");
    for beep in &b {
        let nearest = f
            .iter()
            .map(|flash| beep - flash)
            .fold(f64::INFINITY, |a, d| if d.abs() < a.abs() { d } else { a });
        assert!(
            (-0.005..0.03).contains(&nearest),
            "a beep at {beep} is {nearest:.3} s from its flash: flashes {f:?}, beeps {b:?}"
        );
    }
}

fn join(inputs: &[PathBuf], output: &Path, opts: &clips::Options) -> clips::ClipsOutcome {
    let to = Format::from_path(output).unwrap();
    let plan = clips::plan_merge(&backend(Backend::Ffprobe).path, inputs, to, output, opts)
        .unwrap_or_else(|e| panic!("plan: {}", e.message));
    clips::run(&plan, &backend(Backend::Ffmpeg), false, &mut |_| {})
        .unwrap_or_else(|e| panic!("run: {}", e.message))
}

fn packets(p: &Path, kind: &str) -> usize {
    let select = if kind == "video" { "v:0" } else { "a:0" };
    let (out, _) = run(
        &backend(Backend::Ffprobe).path,
        &[
            "-v",
            "error",
            "-select_streams",
            select,
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
            s(p),
        ],
    );
    out.trim().parse().unwrap()
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn matching_clips_are_copied_and_stay_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let a = clip(dir.path(), "a.mp4", &PLAIN);
    let b = clip(dir.path(), "b.mp4", &PLAIN);
    let c = clip(dir.path(), "c.mov", &PLAIN);
    let out = dir.path().join("out.mp4");

    let o = join(&[a.clone(), b, c], &out, &clips::Options::default());

    assert!(o.copied, "every stream copied");
    assert!(o.notes.is_empty(), "{:?}", o.notes);
    let v = probe(&out);
    assert!((duration(&v) - 6.0).abs() < 0.1, "{}", duration(&v));
    assert_eq!(packets(&out, "video"), 3 * packets(&a, "video"));
    assert_eq!(stream(&v, "video")["codec_name"], "h264");
    assert_in_step(&out, &[1.0, 3.0, 5.0], 3);
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".convkit-"))
        .collect();
    assert!(left.is_empty(), "the scratch folder is removed");
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn phone_clips_with_a_rotation_and_a_variable_rate_copy_when_they_match() {
    let dir = tempfile::tempdir().unwrap();
    let base = clip(dir.path(), "base.mp4", &PLAIN);
    let one = variable_rate(dir.path(), &base, "one.mov");
    let two = variable_rate(dir.path(), &base, "two.mov");
    let up1 = turned(dir.path(), &one, "IMG_1.MOV");
    let up2 = turned(dir.path(), &two, "IMG_2.MOV");
    let out = dir.path().join("trip.mov");

    let o = join(&[up1, up2], &out, &clips::Options::default());

    assert!(o.copied);
    let v = probe(&out);
    let video = stream(&v, "video");
    let rotation = video["side_data_list"][0]["rotation"].as_i64();
    assert_eq!(rotation.map(i64::abs), Some(90), "the turn is kept");
    // The first frame of each clip was dropped, so its flash leads.
    assert_in_step(&out, &[1.0, 3.0], 2);
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn clips_that_differ_are_encoded_to_the_first_and_stay_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let a = clip(dir.path(), "a.mp4", &PLAIN);
    let portrait = clip(
        dir.path(),
        "b0.mov",
        &Spec {
            video: "libx265",
            sound: Some((44_100, 1)),
            ..PLAIN
        },
    );
    let b = turned(dir.path(), &portrait, "b.mov");
    let c = clip(
        dir.path(),
        "c.mp4",
        &Spec {
            size: (160, 120),
            fps: 25,
            sound: None,
            ..PLAIN
        },
    );
    let d = clip(
        dir.path(),
        "d.webm",
        &Spec {
            video: "libvpx-vp9",
            audio: "libopus",
            ..PLAIN
        },
    );
    let e = variable_rate(dir.path(), &a, "e.mov");
    let out = dir.path().join("out.mp4");

    let o = join(&[a, b, c, d, e], &out, &clips::Options::default());

    assert!(!o.copied);
    assert!(
        o.notes[0].starts_with("Re-encoded to 320x180 at 30 fps, as a.mp4 is"),
        "{:?}",
        o.notes
    );
    assert!(o
        .notes
        .iter()
        .any(|n| n == "c.mp4 has no sound, so it is silent in out.mp4."));
    let v = probe(&out);
    let video = stream(&v, "video");
    assert_eq!(
        (video["width"].as_u64(), video["height"].as_u64()),
        (Some(320), Some(180))
    );
    assert_eq!(
        video["avg_frame_rate"], "30/1",
        "a steady 30 fps throughout"
    );
    assert!(video["side_data_list"].is_null(), "turned in the pixels");
    let audio = stream(&v, "audio");
    assert_eq!(audio["sample_rate"], "48000");
    assert_eq!(audio["channels"], 2);
    let length: f64 = audio["duration"].as_str().unwrap().parse().unwrap();
    assert!(
        (length - 10.0).abs() < 0.1,
        "the silence fills c.mp4's part: {length}"
    );
    assert!((duration(&v) - 10.0).abs() < 0.1, "{}", duration(&v));
    // c.mp4 flashes at 5.0 but has no beep.
    assert_in_step(&out, &[1.0, 3.0, 5.0, 7.0, 9.0], 4);
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn many_clips_are_encoded_in_groups_and_stay_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs: Vec<PathBuf> = (0..9)
        .map(|i| clip(dir.path(), &format!("c{i}.mp4"), &PLAIN))
        .collect();
    inputs.push(clip(
        dir.path(),
        "quiet.mp4",
        &Spec {
            sound: None,
            ..PLAIN
        },
    ));
    let out = dir.path().join("all.mp4");

    let o = join(&inputs, &out, &clips::Options::default());

    assert!(!o.copied);
    let v = probe(&out);
    assert!((duration(&v) - 20.0).abs() < 0.15, "{}", duration(&v));
    let flashes: Vec<f64> = (0..10).map(|i| 1.0 + 2.0 * f64::from(i)).collect();
    assert_in_step(&out, &flashes, 9);
}

/// A clip whose sound runs 256 samples (5 ms) past its picture, as an AAC
/// decoder older than ffmpeg 7 gives every AAC clip by keeping the
/// encoder's padding: made here with PCM sound 2.00533 s long, so any
/// ffmpeg shows it.
fn padded(dir: &Path, name: &str) -> PathBuf {
    let out = dir.join(name);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=black:s=320x180:r=30:d=2,drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:\
         enable='between(t,1,1+1/30-0.0001)',format=yuv420p",
        "-f",
        "lavfi",
        "-i",
        "sine=f=1000:r=48000:d=2.00533,volume=enable='not(between(t,1,1.1))':volume=0",
        "-c:v",
        "libx264",
        "-g",
        "30",
        "-c:a",
        "pcm_s16le",
        s(&out),
    ]);
    out
}

/// Without each clip's sound cut to its picture, the padding added up: by
/// the fifth clip its picture was a frame late, and the sound drifted
/// against it by the ms of every join before (seen on Ubuntu's ffmpeg 6.1).
#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn sound_a_few_ms_longer_than_the_picture_still_joins_on_a_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs: Vec<PathBuf> = (0..9)
        .map(|i| padded(dir.path(), &format!("p{i}.mov")))
        .collect();
    inputs.push(clip(
        dir.path(),
        "quiet.mp4",
        &Spec {
            sound: None,
            ..PLAIN
        },
    ));
    let out = dir.path().join("all.mp4");

    let o = join(&inputs, &out, &clips::Options::default());

    assert!(!o.copied);
    assert!((duration(&probe(&out)) - 20.0).abs() < 0.05);
    let flashes: Vec<f64> = (0..10).map(|i| 1.0 + 2.0 * f64::from(i)).collect();
    assert_in_step(&out, &flashes, 9);
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn resize_and_crf_encode_every_clip_within_the_size() {
    let dir = tempfile::tempdir().unwrap();
    let a = clip(dir.path(), "a.mp4", &PLAIN);
    let b = clip(dir.path(), "b.mp4", &PLAIN);
    let out = dir.path().join("small.webm");
    let opts = clips::Options {
        resize: Some("160x".into()),
        crf: Some(40),
        ..Default::default()
    };

    let o = join(&[a, b], &out, &opts);

    assert!(!o.copied);
    let v = probe(&out);
    let video = stream(&v, "video");
    assert_eq!(video["codec_name"], "vp9");
    assert_eq!(video["width"], 160);
    assert_eq!(stream(&v, "audio")["codec_name"], "opus");
    assert_in_step(&out, &[1.0, 3.0], 2);
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn tracks_kept_in_another_order_still_copy() {
    let dir = tempfile::tempdir().unwrap();
    let a = clip(dir.path(), "a.mp4", &PLAIN);
    let swapped = dir.path().join("swapped.mp4");
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-i",
        s(&a),
        "-map",
        "0:a",
        "-map",
        "0:v",
        "-c",
        "copy",
        s(&swapped),
    ]);
    let out = dir.path().join("out.mp4");

    let o = join(&[a, swapped], &out, &clips::Options::default());

    assert!(o.copied);
    assert_in_step(&out, &[1.0, 3.0], 2);
}

/// Songs: `sine` at `rate`, encoded as MP3.
fn song(dir: &Path, name: &str, rate: u32, title: &str) -> PathBuf {
    let out = dir.join(name);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("sine=f=440:r={rate}:d=2"),
        "-c:a",
        "libmp3lame",
        "-q:a",
        "2",
        "-metadata",
        "artist=Band",
        "-metadata",
        &format!("title={title}"),
        s(&out),
    ]);
    out
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn songs_are_copied_with_the_tags_they_share() {
    let dir = tempfile::tempdir().unwrap();
    let a = song(dir.path(), "01.mp3", 44_100, "One");
    let b = song(dir.path(), "02.mp3", 44_100, "Two");
    let out = dir.path().join("album.mp3");

    let o = join(&[a.clone(), b], &out, &clips::Options::default());

    assert!(o.copied);
    assert!(
        o.warnings.is_empty(),
        "the joins' DTS fix-ups are routine: {:?}",
        o.warnings
    );
    let v = probe(&out);
    assert!((duration(&v) - 4.0).abs() < 0.15, "{}", duration(&v));
    assert_eq!(packets(&out, "audio"), 2 * packets(&a, "audio"));
    assert_eq!(v["format"]["tags"]["artist"], "Band");
    assert!(
        v["format"]["tags"]["title"].is_null(),
        "a title names one song"
    );
}

#[test]
#[ignore = "requires ffmpeg and ffprobe; run with --ignored"]
fn songs_at_another_rate_are_resampled_to_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let a = song(dir.path(), "a.mp3", 44_100, "A");
    let b = song(dir.path(), "b.mp3", 48_000, "B");
    let talk = clip(dir.path(), "talk.mp4", &PLAIN);
    let out = dir.path().join("mix.m4a");

    let o = join(&[a, b, talk], &out, &clips::Options::default());

    assert!(!o.copied);
    let v = probe(&out);
    let audio = stream(&v, "audio");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["sample_rate"], "44100");
    assert!((duration(&v) - 6.0).abs() < 0.15, "{}", duration(&v));
}

// --- PDFs and pictures ----------------------------------------------------

/// Each page's shown size in points.
fn page_sizes(p: &Path) -> Vec<(i64, i64)> {
    let (out, _) = run(
        &backend(Backend::Qpdf).path,
        &["--json", "--json-key=pages", "--json-key=qpdf", s(p)],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    (1..=v["pages"].as_array().unwrap().len() as u32)
        .map(|n| {
            let (w, h) = pages::page_size(&v, n).unwrap();
            (w.round() as i64, h.round() as i64)
        })
        .collect()
}

fn page_count(p: &Path) -> usize {
    let (out, _) = run(&backend(Backend::Qpdf).path, &["--show-npages", s(p)]);
    out.trim().parse().unwrap()
}

/// A `w`x`h` PNG of one colour, at 72 dpi.
fn picture(dir: &Path, name: &str, (w, h): (u32, u32)) -> PathBuf {
    let out = dir.join(name);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("color=teal:s={w}x{h}"),
        "-frames:v",
        "1",
        s(&out),
    ]);
    out
}

fn merge_pages(inputs: &[PathBuf], output: &Path) -> convkit_core::pdf::PdfOutcome {
    let resolver = Resolver::new();
    let qpdf = backend(Backend::Qpdf);
    let plan = pages::plan_merge(&qpdf.path, inputs, output, &mut |_: &Path, _| None)
        .unwrap_or_else(|e| panic!("plan: {}", e.message));
    pages::run(
        &plan,
        &resolver,
        &backend(Backend::Typst),
        &qpdf,
        false,
        &mut |_| {},
    )
    .unwrap_or_else(|e| panic!("run: {}", e.message))
}

#[test]
#[ignore = "requires qpdf, typst and ffmpeg; run with --ignored"]
fn pictures_take_the_size_of_the_pdf_page_beside_them() {
    let dir = tempfile::tempdir().unwrap();
    // sample.pdf's pages are 210, 220 and 230 points wide, 297 high.
    let pdf = dir.path().join("report.pdf");
    std::fs::copy(fixture("sample.pdf"), &pdf).unwrap();
    let cover = picture(dir.path(), "cover.png", (1600, 1200));
    let tall = picture(dir.path(), "tall.png", (300, 500));
    let tiny = dir.path().join("photo.jpg");
    std::fs::copy(fixture("rotated.jpg"), &tiny).unwrap();
    let out = dir.path().join("out.pdf");

    let o = merge_pages(&[cover, pdf.clone(), tall, tiny.clone(), pdf], &out);

    assert_eq!(page_count(&out), 1 + 3 + 2 + 3);
    assert_eq!(o.outputs[0].pages.len(), 9);
    let sizes = page_sizes(&out);
    // The cover is landscape: the first page after it, turned.
    assert_eq!(sizes[0], (297, 210));
    assert_eq!(&sizes[1..4], &[(210, 297), (220, 297), (230, 297)]);
    // Beside the last page before them, each upright as it is shown.
    assert_eq!(sizes[4], (230, 297), "the tall picture");
    assert_eq!(sizes[5], (230, 297), "the photo, upright by its EXIF");
    // The JPEG goes in as it is.
    let jpeg = std::fs::read(&tiny).unwrap();
    let merged = std::fs::read(&out).unwrap();
    assert!(
        merged.windows(jpeg.len()).any(|w| w == jpeg.as_slice()),
        "the JPEG's bytes are in the PDF unchanged"
    );
}

/// A HEIC goes in converted to JPEG, which keeps its EXIF, GPS included,
/// and the PDF embeds that JPEG as it is. So the note must say so exactly
/// when the location is in the PDF, whatever this ImageMagick does with a
/// HEIC's EXIF: ImageMagick 6, as Debian and Ubuntu ship it, keeps it
/// behind the HEIF item's own offset, where its own reader misses it.
#[test]
#[ignore = "requires qpdf, typst and ImageMagick; run with --ignored"]
fn a_heic_among_pdfs_is_converted_and_its_location_noted() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("form.pdf");
    std::fs::copy(fixture("sample.pdf"), &pdf).unwrap();
    let out = dir.path().join("out.pdf");

    let o = merge_pages(&[pdf, fixture("photo.heic")], &out);

    assert_eq!(page_count(&out), 4);
    let sizes = page_sizes(&out);
    let (w, h) = sizes[3];
    assert_eq!((w.max(h), w.min(h)), (297, 230), "the last page's size");
    // The photo's GPS latitude as its big-endian EXIF stores it: 35/1, 43/1.
    let latitude = b"\x00\x00\x00\x23\x00\x00\x00\x01\x00\x00\x00\x2b\x00\x00\x00\x01";
    let kept = std::fs::read(&out)
        .unwrap()
        .windows(latitude.len())
        .any(|w| w == latitude);
    let noted = o
        .notes
        .iter()
        .any(|n| n.starts_with("photo.heic records where it was taken"));
    assert_eq!(
        noted, kept,
        "location in the PDF: {kept}; notes: {:?}",
        o.notes
    );
}

#[test]
#[ignore = "requires qpdf, typst and ffmpeg; run with --ignored"]
fn a_locked_pdf_among_pictures_is_opened_and_the_merge_can_be_locked() {
    use convkit_core::pdf::{Lock, Password};
    let dir = tempfile::tempdir().unwrap();
    let qpdf = backend(Backend::Qpdf);
    let locked = dir.path().join("locked.pdf");
    run(
        &qpdf.path,
        &[
            "--encrypt",
            "--user-password=open",
            "--owner-password=owner",
            "--bits=256",
            "--",
            s(&fixture("sample.pdf")),
            s(&locked),
        ],
    );
    let cover = picture(dir.path(), "cover.png", (1600, 1200));
    let out = dir.path().join("out.pdf");

    let mut asked = 0;
    let mut plan = pages::plan_merge(
        &qpdf.path,
        &[cover, locked],
        &out,
        &mut |_: &Path, wrong| {
            asked += 1;
            (!wrong).then(|| Password::new("open").unwrap())
        },
    )
    .unwrap_or_else(|e| panic!("plan: {}", e.message));
    assert_eq!(asked, 1, "asked once for the locked PDF");
    plan.set_lock(Lock::new(Password::new("new").unwrap(), &[]).unwrap());
    let shown = plan.commands().unwrap();
    let join = &shown.last().unwrap().1;
    assert!(
        join.iter()
            .all(|a| !a.ends_with("=open") && !a.ends_with("=new")),
        "no password is shown: {join:?}"
    );
    pages::run(
        &plan,
        &Resolver::new(),
        &backend(Backend::Typst),
        &qpdf,
        false,
        &mut |_| {},
    )
    .unwrap_or_else(|e| panic!("run: {}", e.message));

    let needs = Command::new(&qpdf.path)
        .args(["--requires-password", s(&out)])
        .status()
        .unwrap();
    assert_eq!(needs.code(), Some(0), "the merge needs a password to open");
    let (pages, _) = run(&qpdf.path, &["--password=new", "--show-npages", s(&out)]);
    assert_eq!(pages.trim(), "4");
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn a_broken_picture_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("a.pdf");
    std::fs::copy(fixture("sample.pdf"), &pdf).unwrap();
    let bad = dir.path().join("scan 1.png");
    std::fs::write(&bad, b"not a png at all").unwrap();
    let out = dir.path().join("out.pdf");
    let qpdf = backend(Backend::Qpdf);
    let plan = pages::plan_merge(&qpdf.path, &[pdf, bad], &out, &mut |_: &Path, _| None).unwrap();

    let e = pages::run(
        &plan,
        &Resolver::new(),
        &backend(Backend::Typst),
        &qpdf,
        false,
        &mut |_| {},
    )
    .unwrap_err();

    assert!(e.message.contains("scan 1.png"), "{}", e.message);
    assert!(!e.message.contains("convkit-in-"), "{}", e.message);
    assert!(!out.exists());
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        2,
        "nothing left behind"
    );
}
