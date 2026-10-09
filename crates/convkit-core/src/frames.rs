//! One frame, or a file's loudness, read from ffmpeg over a pipe: what
//! `conv trim` draws. Nothing is written to disk, and nothing here prints.

use std::ffi::OsString;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Child, Stdio};

use crate::procutil::backend_command;
use crate::{ConvError, ErrorCode, Result};

/// What to grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Raw RGB at exactly this size.
    Rgb { width: u32, height: u32 },
    /// A PNG this wide, its height by aspect.
    Png { width: u32 },
}

/// One frame's pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pixels {
    Rgb {
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    Png(Vec<u8>),
}

/// Options every read here starts with. `-nostdin` because ffmpeg reads
/// its stdin for interactive commands (`q` quits), and under `conv trim`
/// that stdin is the terminal: it would take the user's keys. `-v error`
/// keeps stderr down to what a failure needs.
const QUIET: [&str; 3] = ["-v", "error", "-nostdin"];

/// The ffmpeg argv for one frame at `at_ms`.
///
/// `-ss` before `-i` seeks rather than decoding the whole file up to the
/// frame, and still lands on the exact frame (ffmpeg decodes from the
/// keyframe before and drops what comes first). Only the first video
/// stream is read, so cover art in a second one is never taken for the
/// picture. `flags=area` averages the pixels into the small picture, so a
/// 1x1 grab is the frame's mean colour, which is what the video bar draws.
pub fn grab_args(input: &Path, at_ms: u64, want: Want) -> Vec<OsString> {
    let mut args: Vec<OsString> = QUIET.iter().map(OsString::from).collect();
    args.push("-ss".into());
    args.push(format!("{}.{:03}", at_ms / 1000, at_ms % 1000).into());
    args.push("-i".into());
    args.push(input.as_os_str().to_owned());
    for a in ["-map", "0:v:0", "-frames:v", "1", "-vf"] {
        args.push(a.into());
    }
    match want {
        Want::Rgb { width, height } => {
            args.push(format!("scale={width}:{height}:flags=area").into());
            // Three bytes a pixel, nothing else: the reader knows the size.
            for a in ["-f", "rawvideo", "-pix_fmt", "rgb24"] {
                args.push(a.into());
            }
        }
        Want::Png { width } => {
            args.push(format!("scale={width}:-2").into());
            for a in ["-c:v", "png", "-f", "image2pipe"] {
                args.push(a.into());
            }
        }
    }
    args.push("pipe:1".into());
    args
}

/// Runs ffmpeg for one frame and reads it from its stdout.
pub fn grab(ffmpeg: &Path, input: &Path, at_ms: u64, want: Want) -> Result<Pixels> {
    let child = spawn(ffmpeg, grab_args(input, at_ms, want))?;
    let (data, stderr, ok) = drain(child)?;
    let none = || {
        let detail = stderr.trim();
        let message = if detail.is_empty() {
            format!(
                "ffmpeg read no frame at {} in {}",
                crate::trim::format_time(at_ms),
                input.display()
            )
        } else {
            format!("ffmpeg read no frame of {}: {detail}", input.display())
        };
        ConvError::new(ErrorCode::ConversionFailed, message)
    };
    match want {
        Want::Rgb { width, height } => {
            if !ok || data.len() != width as usize * height as usize * 3 {
                return Err(none());
            }
            Ok(Pixels::Rgb {
                width,
                height,
                data,
            })
        }
        Want::Png { .. } => {
            if !ok || !data.starts_with(b"\x89PNG") {
                return Err(none());
            }
            Ok(Pixels::Png(data))
        }
    }
}

/// Loudness is reported per bucket of this many milliseconds.
pub const BUCKET_MS: u64 = 10;

/// 8 kHz mono: plenty for a loudness bar, and only 16 KB a second through
/// the pipe.
const RATE: u64 = 8_000;
const BUCKET_BYTES: usize = (RATE * BUCKET_MS / 1000) as usize * 2;

/// Decodes the first audio track and calls `each` with the RMS of every
/// 10 ms bucket (linear, 0 to 1), a batch at a time as ffmpeg delivers
/// them. Returns once the file has been read to its end, or as soon as
/// `each` breaks, when ffmpeg is stopped rather than left decoding.
pub fn loudness(
    ffmpeg: &Path,
    input: &Path,
    each: &mut dyn FnMut(&[f32]) -> ControlFlow<()>,
) -> Result<()> {
    let mut args: Vec<OsString> = QUIET.iter().map(OsString::from).collect();
    args.push("-i".into());
    args.push(input.as_os_str().to_owned());
    for a in [
        "-map", "0:a:0", "-ac", "1", "-ar", "8000", "-f", "s16le", "pipe:1",
    ] {
        args.push(a.into());
    }
    let mut child = spawn(ffmpeg, args)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = read_in_background(child.stderr.take().expect("stderr is piped"));
    let mut buckets = Buckets::default();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = stdout.read(&mut buf).map_err(io_err)?;
        if n == 0 {
            break;
        }
        let batch = buckets.push(&buf[..n]);
        if !batch.is_empty() && each(&batch).is_break() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    }
    if let Some(last) = buckets.finish() {
        let _ = each(&[last]);
    }
    let status = child.wait().map_err(io_err)?;
    if !status.success() {
        let detail = stderr.join().unwrap_or_default();
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "ffmpeg could not read the sound of {}: {}",
                input.display(),
                detail.trim()
            ),
        ));
    }
    Ok(())
}

/// RMS of 16-bit samples, linear: 0 for silence, 1 for full scale.
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / samples.len() as f64).sqrt() / 32_767.0) as f32
}

/// A linear level in dBFS, floored at -90.
pub fn dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        return -90.0;
    }
    (20.0 * rms.log10()).max(-90.0)
}

/// Splits a stream of s16le bytes into 10 ms buckets, however the reads
/// happen to fall.
#[derive(Debug, Default)]
pub struct Buckets {
    pending: Vec<u8>,
}

impl Buckets {
    /// The RMS of every whole bucket the bytes complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<f32> {
        self.pending.extend_from_slice(bytes);
        let whole = self.pending.len() / BUCKET_BYTES * BUCKET_BYTES;
        let out = self.pending[..whole]
            .chunks_exact(BUCKET_BYTES)
            .map(|b| rms(&samples(b)))
            .collect();
        self.pending.drain(..whole);
        out
    }

    /// The short last bucket, if the stream ended inside one.
    pub fn finish(self) -> Option<f32> {
        (self.pending.len() >= 2).then(|| rms(&samples(&self.pending)))
    }
}

fn samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn spawn(ffmpeg: &Path, args: Vec<OsString>) -> Result<Child> {
    backend_command(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run ffmpeg: {e}"),
            )
        })
}

/// Reads stdout to its end while stderr is read alongside, so neither pipe
/// fills and stalls the other; then waits.
fn drain(mut child: Child) -> Result<(Vec<u8>, String, bool)> {
    let stderr = read_in_background(child.stderr.take().expect("stderr is piped"));
    let mut data = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_end(&mut data)
        .map_err(io_err)?;
    let ok = child.wait().map_err(io_err)?.success();
    Ok((data, stderr.join().unwrap_or_default(), ok))
}

fn read_in_background(mut pipe: std::process::ChildStderr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    })
}

fn io_err(e: std::io::Error) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("reading from ffmpeg failed: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_raw_frame_is_seeked_scaled_and_piped() {
        assert_eq!(
            strs(grab_args(
                Path::new("talk.mp4"),
                62_500,
                Want::Rgb {
                    width: 120,
                    height: 68
                }
            )),
            [
                "-v",
                "error",
                "-nostdin",
                "-ss",
                "62.500",
                "-i",
                "talk.mp4",
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-vf",
                "scale=120:68:flags=area",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1"
            ]
        );
    }

    #[test]
    fn a_png_frame_keeps_its_aspect() {
        let args = strs(grab_args(
            Path::new("talk.mp4"),
            0,
            Want::Png { width: 640 },
        ));
        assert!(
            args.windows(2).any(|w| w == ["-vf", "scale=640:-2"]),
            "{args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["-c:v", "png"]), "{args:?}");
        assert!(
            args.windows(2).any(|w| w == ["-f", "image2pipe"]),
            "{args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
    }

    #[test]
    fn rms_runs_from_silence_to_full_scale() {
        assert_eq!(rms(&[0; 80]), 0.0);
        assert!((rms(&[32767, -32767, 32767, -32767]) - 1.0).abs() < 0.001);
        let sine: Vec<i16> = (0..800)
            .map(|i| (16384.0 * (i as f32 * std::f32::consts::TAU / 80.0).sin()) as i16)
            .collect();
        assert!((rms(&sine) - 0.3536).abs() < 0.005, "{}", rms(&sine));
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn dbfs_is_floored() {
        assert_eq!(dbfs(1.0), 0.0);
        assert!((dbfs(0.5) + 6.02).abs() < 0.01);
        assert_eq!(dbfs(0.0), -90.0);
        assert_eq!(dbfs(1e-9), -90.0);
    }

    #[test]
    fn buckets_ignore_where_reads_fall() {
        // 10 ms at 8 kHz mono s16le is 160 bytes.
        let mut b = Buckets::default();
        assert!(b.push(&[0; 100]).is_empty());
        assert_eq!(
            b.push(&[0; 300]).len(),
            2,
            "400 bytes: two buckets, 80 left"
        );
        assert_eq!(
            b.finish(),
            Some(0.0),
            "the last 80 bytes are a short bucket"
        );
        let mut b = Buckets::default();
        assert_eq!(b.push(&[0; 160]).len(), 1);
        assert_eq!(b.finish(), None);
    }
}
