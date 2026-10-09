//! Marked clips into conversions: each one the job and tuning that
//! `conv FILE --start T --end T` would build, so the clips run through the
//! same code as the flags.

use std::path::Path;

use convkit_core::trim::{format_time, name_suffix, Range, Time};
use convkit_core::{registry, ConvError, ErrorCode, Format, Kind, Tuning};

use super::session::{Clip, Keeps};
use crate::input::Job;

/// The formats the clips of one file are written in: `video` for a clip
/// with a picture (`None` when there is none to keep), `audio` for one
/// with sound alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub video: Option<Format>,
    pub audio: Format,
}

fn refuse(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// What the clips of `input` are written as, given `--to`: a clip with a
/// picture keeps the file's own format (or `--to`); one with sound alone
/// is m4a from a video, and the file's own format (or an audio `--to`)
/// from an audio file. Refused, with what to do, for a file conv trim
/// cannot cut.
pub fn targets(input: &Path, from: Format, to: Option<Format>) -> Result<Target, ConvError> {
    let name = input.display();
    let still = |f: Format| f.kind() == Kind::Image && f != Format::Gif;
    if still(from) {
        return Err(refuse(format!(
            "conv trim cuts video and audio; {name} is a still image"
        )));
    }
    if from.kind() == Kind::Document {
        return Err(refuse(format!(
            "conv trim cuts video and audio; {name} is a document"
        )));
    }
    if let Some(to) = to.filter(|&t| still(t) || t.kind() == Kind::Document) {
        let what = if still(to) {
            "a still image"
        } else {
            "a document"
        };
        return Err(refuse(format!(
            "conv trim writes video and audio clips; --to {} is {what}",
            to.ext()
        )));
    }
    let audio = |f: Format| f.kind() == Kind::Audio;
    if from.kind() == Kind::Audio {
        return match to {
            Some(t) if !audio(t) => Err(refuse(format!(
                "--to {} needs a picture, and {name} has none; use an audio format",
                t.ext()
            ))),
            Some(t) => Ok(Target {
                video: None,
                audio: t,
            }),
            None => Ok(Target {
                video: None,
                audio: from,
            }),
        };
    }
    let video = match to {
        Some(t) if audio(t) => {
            return Ok(Target {
                video: None,
                audio: t,
            })
        }
        Some(t) if registry::takes_range(from, t) => t,
        Some(t) => return Err(ConvError::unsupported_pair(from, t)),
        // gif and avi have a timeline but no recipe into themselves.
        None if registry::takes_range(from, from) => from,
        None => {
            return Err(refuse(format!(
                "conv trim keeps {name}'s own format, and {} can't be cut into itself; \
                 add --to mp4",
                from.ext()
            )))
        }
    };
    Ok(Target {
        video: Some(video),
        audio: Format::M4a,
    })
}

/// One clip as a conversion: the job and the tuning `conv FILE --start
/// --end` would build, written beside the input or in `outdir`. The name
/// carries the range, as a cut into its own format does, and `-silent` for
/// a clip with the picture alone, so it never shares a name with the same
/// range kept whole.
pub fn job(input: &Path, outdir: Option<&Path>, clip: &Clip, target: &Target) -> (Job, Tuning) {
    let (to, mute) = match (target.video, clip.keeps) {
        (None, _) | (Some(_), Keeps::Audio) => (target.audio, false),
        // A GIF has no sound to drop.
        (Some(v), Keeps::Video) => (v, v != Format::Gif),
        (Some(v), Keeps::Both) => (v, false),
    };
    let time = |ms| Time {
        ms,
        from_end: false,
        text: format_time(ms),
    };
    let range = Range::new(Some(time(clip.start_ms)), Some(time(clip.end_ms)), None)
        .expect("a clip starts before it ends")
        .expect("two times make a range");
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let silent = if mute { "-silent" } else { "" };
    let name = format!("{stem}-{}{silent}.{}", name_suffix(&range), to.ext());
    let output = match outdir {
        Some(dir) => dir.join(name),
        None => input.with_file_name(name),
    };
    let job = Job {
        inputs: vec![input.to_path_buf()],
        output,
        from: Format::from_path(input).expect("targets() has read the input's format"),
        to,
    };
    let tuning = Tuning {
        range: Some(range),
        mute,
        ..Tuning::default()
    };
    (job, tuning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trim::session::Keeps;
    use convkit_core::ErrorCode;
    use std::path::PathBuf;

    fn clip(start_ms: u64, end_ms: u64, keeps: Keeps) -> Clip {
        Clip {
            start_ms,
            end_ms,
            keeps,
        }
    }

    fn written(input: &str, to: Option<Format>, c: Clip) -> (PathBuf, Format, Tuning) {
        let input = Path::new(input);
        let from = Format::from_path(input).unwrap();
        let target = targets(input, from, to).unwrap();
        let (job, tuning) = job(input, None, &c, &target);
        assert_eq!(job.inputs, [input.to_path_buf()]);
        assert_eq!(job.from, from);
        (job.output, job.to, tuning)
    }

    #[test]
    fn a_clip_of_a_video_keeps_what_its_bar_chose() {
        let (out, to, t) = written("talk.mp4", None, clip(62_000, 70_000, Keeps::Both));
        assert_eq!(
            (out, to, t.mute),
            (PathBuf::from("talk-1m02s-1m10s.mp4"), Format::Mp4, false)
        );
        let r = t.range.unwrap();
        assert_eq!(r.start.as_ref().map(|s| s.ms), Some(62_000));
        let (out, to, t) = written("talk.mp4", None, clip(62_000, 70_000, Keeps::Video));
        assert_eq!(
            (out, to, t.mute),
            (
                PathBuf::from("talk-1m02s-1m10s-silent.mp4"),
                Format::Mp4,
                true
            )
        );
        let (out, to, t) = written("talk.mkv", None, clip(62_000, 70_000, Keeps::Audio));
        assert_eq!(
            (out, to, t.mute),
            (PathBuf::from("talk-1m02s-1m10s.m4a"), Format::M4a, false)
        );
    }

    #[test]
    fn fractions_and_the_start_of_the_file_are_named_plainly() {
        let (out, ..) = written("talk.mp4", None, clip(0, 62_500, Keeps::Both));
        assert_eq!(out, PathBuf::from("talk-0s-1m02.5s.mp4"));
    }

    #[test]
    fn to_gif_writes_every_picture_clip_as_a_gif_and_sound_as_m4a() {
        let gif = Some(Format::Gif);
        let (out, to, t) = written("talk.mp4", gif, clip(1_000, 3_000, Keeps::Both));
        assert_eq!(
            (out, to, t.mute),
            (PathBuf::from("talk-1s-3s.gif"), Format::Gif, false)
        );
        let (out, _, t) = written("talk.mp4", gif, clip(1_000, 3_000, Keeps::Video));
        assert_eq!(
            out,
            PathBuf::from("talk-1s-3s.gif"),
            "a GIF is silent already"
        );
        assert!(!t.mute);
        let (out, ..) = written("talk.mp4", gif, clip(1_000, 3_000, Keeps::Audio));
        assert_eq!(out, PathBuf::from("talk-1s-3s.m4a"));
    }

    #[test]
    fn an_audio_to_keeps_only_sound() {
        let t = targets(Path::new("talk.mp4"), Format::Mp4, Some(Format::Mp3)).unwrap();
        assert_eq!(
            t,
            Target {
                video: None,
                audio: Format::Mp3
            }
        );
        let (out, to, _) = written(
            "talk.mp4",
            Some(Format::Mp3),
            clip(1_000, 3_000, Keeps::Both),
        );
        assert_eq!((out, to), (PathBuf::from("talk-1s-3s.mp3"), Format::Mp3));
    }

    #[test]
    fn an_audio_file_keeps_its_own_format() {
        let (out, to, _) = written("memo.m4a", None, clip(10_000, 40_000, Keeps::Both));
        assert_eq!((out, to), (PathBuf::from("memo-10s-40s.m4a"), Format::M4a));
        let (out, to, _) = written(
            "memo.m4a",
            Some(Format::Mp3),
            clip(10_000, 40_000, Keeps::Both),
        );
        assert_eq!((out, to), (PathBuf::from("memo-10s-40s.mp3"), Format::Mp3));
    }

    #[test]
    fn an_outdir_holds_the_clips() {
        let input = Path::new("videos/talk.mp4");
        let target = targets(input, Format::Mp4, None).unwrap();
        let (job, _) = job(
            input,
            Some(Path::new("clips")),
            &clip(5_000, 9_000, Keeps::Both),
            &target,
        );
        assert_eq!(job.output, PathBuf::from("clips/talk-5s-9s.mp4"));
        let (job, _) = job_in_place(input, &target);
        assert_eq!(job.output, PathBuf::from("videos/talk-5s-9s.mp4"));
    }

    fn job_in_place(input: &Path, target: &Target) -> (Job, Tuning) {
        job(input, None, &clip(5_000, 9_000, Keeps::Both), target)
    }

    #[test]
    fn what_conv_trim_cannot_cut_is_refused_with_what_to_do() {
        let refused = |input: &str, to: Option<Format>| {
            let p = Path::new(input);
            let e = targets(p, Format::from_path(p).unwrap(), to).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation, "{input}");
            e.message
        };
        assert_eq!(
            refused("photo.png", None),
            "conv trim cuts video and audio; photo.png is a still image"
        );
        assert_eq!(
            refused("report.pdf", None),
            "conv trim cuts video and audio; report.pdf is a document"
        );
        assert_eq!(
            refused("anim.gif", None),
            "conv trim keeps anim.gif's own format, and gif can't be cut into itself; add --to mp4"
        );
        assert_eq!(
            refused("old.avi", None),
            "conv trim keeps old.avi's own format, and avi can't be cut into itself; add --to mp4"
        );
        assert_eq!(
            refused("talk.mp4", Some(Format::Png)),
            "conv trim writes video and audio clips; --to png is a still image"
        );
        assert_eq!(
            refused("memo.m4a", Some(Format::Mp4)),
            "--to mp4 needs a picture, and memo.m4a has none; use an audio format"
        );
    }

    #[test]
    fn a_gif_with_to_mp4_is_cut() {
        let (out, to, _) = written("anim.gif", Some(Format::Mp4), clip(0, 2_000, Keeps::Both));
        assert_eq!((out, to), (PathBuf::from("anim-0s-2s.mp4"), Format::Mp4));
    }
}
