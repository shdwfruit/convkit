//! `conv trim FILE`: an interactive cutter in the terminal. The frame under
//! a slider, a video bar of frame colours and a loudness bar; clips marked
//! with `c` and written, on `w`, as `conv FILE --start --end` would write
//! them.
//!
//! Three workers feed one event loop: one grabs the frame under the
//! slider (the latest position wins), one grabs a colour for each column
//! of the video bar, and one reads the loudness. Keys arrive on a fourth
//! thread. Everything goes through one channel, and the screen is redrawn
//! after each batch of events.

pub mod clips;
pub mod draw;
pub mod session;
pub mod term;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Instant;

use convkit_core::frames::{self, Pixels, Want};
use convkit_core::{
    plan, probe, Backend, ConvError, ConversionPlan, ErrorCode, Format, MediaProbe,
};
use rayon::prelude::*;

use crate::cli::{Cli, Command};
use crate::{batch, render};
use draw::{Graphics, ImageBox, Info, Layout};
use session::{Input, Outcome, Session};

enum Event {
    Key(Input, Instant),
    Frame { generation: u64, pixels: Pixels },
    Strip { at_ms: u64, rgb: [u8; 3] },
    Loudness(Vec<f32>),
}

pub fn run(cli: &Cli) -> i32 {
    let Some(Command::Trim {
        file,
        to,
        outdir,
        overwrite,
        dry_run,
        graphics,
    }) = &cli.command
    else {
        unreachable!("main dispatches only Command::Trim here");
    };
    let args = Args {
        file,
        to: to.as_deref(),
        outdir: outdir.as_deref(),
        overwrite: *overwrite,
        dry_run: *dry_run,
        graphics: graphics.as_deref(),
    };
    match trim(cli, &args) {
        Ok(code) => code,
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

struct Args<'a> {
    file: &'a Path,
    to: Option<&'a str>,
    outdir: Option<&'a Path>,
    overwrite: bool,
    dry_run: bool,
    graphics: Option<&'a str>,
}

/// Everything that can be refused is refused before the screen is touched,
/// in the order that costs least: the file's format, then the terminal,
/// then the backends and the probe.
fn trim(cli: &Cli, args: &Args) -> Result<i32, ConvError> {
    let file = args.file;
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    let from = Format::from_ext(ext).ok_or_else(|| ConvError::unknown_format(ext))?;
    let to = args
        .to
        .map(|t| Format::from_ext(t).ok_or_else(|| ConvError::unknown_format(t)))
        .transpose()?;
    let target = clips::targets(file, from, to)?;
    if cli.json || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "conv trim needs a terminal; use conv FILE --start T --end T to cut without one",
        ));
    }
    if !file.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", file.display()),
        ));
    }
    let resolver = cli.resolver();
    let ffprobe = resolver.resolve(Backend::Ffprobe)?.path;
    let ffmpeg = resolver.resolve(Backend::Ffmpeg)?.path;
    let probe = probe::run(&ffprobe, file)?;
    let name = file.display();
    let Some(duration) = probe.duration_ms.filter(|&d| d > 0) else {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "cannot read the length of {name}, so the bars have nothing to span; \
                 use conv {name} --start T --end T, which needs none"
            ),
        ));
    };
    let has_video = target.video.is_some() && probe.video_streams > 0;
    let has_audio = !probe.audio_codecs.is_empty();
    if !has_video && !has_audio {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} has no picture or sound for conv trim to cut"),
        ));
    }
    let frame_ms = probe
        .frame_rate
        .filter(|&(n, _)| n > 0)
        .map(|(n, d)| 1000 * u64::from(d) / u64::from(n));
    let session = Session::new(duration, frame_ms, has_video, has_audio);
    // A GIF has no sound to keep or drop: its clips keep the picture alone.
    let mut session = if target.video == Some(Format::Gif) {
        session.picture_alone()
    } else {
        session
    };
    let size = probe.display_dimensions().filter(|_| has_video);
    let info = Info {
        name: file
            .file_name()
            .map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned()),
        size,
        fps: probe.frame_rate.filter(|_| has_video).map(fps_words),
    };
    let graphics = term::choose(args.graphics, &|k| std::env::var(k).ok());
    let source = Source {
        ffmpeg: &ffmpeg,
        file,
        size,
        frame_ms: frame_ms.unwrap_or(40),
        has_audio,
    };
    let writing = Writing {
        file,
        outdir: args.outdir,
        target: &target,
        probe: &probe,
        overwrite: args.overwrite,
        dry_run: args.dry_run,
    };
    let overwrite = match interact(&mut session, &info, &source, graphics, &writing) {
        Outcome::Write => args.overwrite,
        Outcome::Replace => true,
        Outcome::Quit | Outcome::Continue => return Ok(0),
    };
    write(cli, &writing, overwrite, session.clips())
}

fn fps_words((n, d): (u32, u32)) -> String {
    if d != 0 && n % d == 0 {
        (n / d).to_string()
    } else {
        format!("{:.2}", f64::from(n) / f64::from(d.max(1)))
    }
}

/// What the workers read.
struct Source<'a> {
    ffmpeg: &'a Path,
    file: &'a Path,
    /// The picture's displayed size; `None` when there is no picture.
    size: Option<(u32, u32)>,
    frame_ms: u64,
    has_audio: bool,
}

/// The screen as last drawn, and what the workers have sent.
struct Shown {
    graphics: Graphics,
    size: (u16, u16),
    layout: Option<Layout>,
    picture: Option<(u64, Pixels)>,
    picture_dirty: bool,
    strip: HashMap<u64, [u8; 3]>,
    strip_asked: Vec<u64>,
    loudness: Vec<f32>,
    frame_asked: Option<(u64, (u16, u16))>,
}

/// Runs the session until it is written or quit. The screen is held only
/// for the length of this call.
fn interact(
    session: &mut Session,
    info: &Info,
    source: &Source,
    graphics: Graphics,
    writing: &Writing,
) -> Outcome {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut screen = term::Screen::enter(graphics);
    let keys = tx.clone();
    screen.read_keys(move |input, at| keys.send(Event::Key(input, at)).is_ok());
    // Set when the session ends, so a worker stops spawning ffmpeg.
    let done = Arc::new(AtomicBool::new(false));
    let frames = source
        .size
        .map(|_| frame_worker(source, tx.clone(), Arc::clone(&done)));
    let strip_generation = Arc::new(AtomicU64::new(0));
    let strips = source.size.map(|_| {
        strip_worker(
            source,
            tx.clone(),
            Arc::clone(&strip_generation),
            Arc::clone(&done),
        )
    });
    if source.has_audio {
        loudness_worker(source, tx.clone(), Arc::clone(&done));
    }
    drop(tx);

    let mut shown = Shown {
        graphics,
        size: (0, 0),
        layout: None,
        picture: None,
        picture_dirty: false,
        strip: HashMap::new(),
        strip_asked: Vec::new(),
        loudness: Vec::new(),
        frame_asked: None,
    };
    let outcome = loop {
        redraw(session, info, &mut shown);
        // Ask for what the screen now needs: a frame for a new slider
        // position or a new picture size, colours for new columns.
        if let (Some(frames), Some(image)) = (&frames, shown.layout.and_then(|l| l.image)) {
            let asked = (session.generation(), shown.size);
            if shown.frame_asked != Some(asked) {
                let at = session
                    .slider_ms()
                    .min(session.duration_ms().saturating_sub(source.frame_ms));
                let want = want_for(graphics, image, source.size.expect("a picture"));
                let _ = frames.send((session.generation(), at, want));
                shown.frame_asked = Some(asked);
            }
        }
        if let (Some(strips), Some(layout)) = (&strips, shown.layout) {
            let times = draw::column_times(session.view(), draw::bar_width(layout.cols));
            if times != shown.strip_asked {
                let missing: Vec<u64> = times
                    .iter()
                    .filter(|t| !shown.strip.contains_key(t))
                    .copied()
                    .collect();
                let generation = strip_generation.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = strips.send((generation, missing));
                shown.strip_asked = times;
            }
        }

        let Ok(first) = rx.recv() else {
            break Outcome::Quit;
        };
        let mut finished = None;
        for event in take_batch(first, &rx, BATCH) {
            match event {
                Event::Key(input, at) => {
                    let marked = session.clips().len();
                    match session.input(input, at) {
                        Outcome::Continue => {
                            if session.clips().len() > marked {
                                let clip = *session.clips().last().expect("a clip just marked");
                                if let Err(e) = writing.check(&clip) {
                                    session.refuse_last(&format!("Not marked: {}.", e.message));
                                }
                            }
                        }
                        Outcome::Write => match writing.ready(session.clips()) {
                            Ready::Go => {
                                finished = Some(Outcome::Write);
                                break;
                            }
                            Ready::Refuse(why) => session.tell(why),
                            Ready::Ask(question) => session.ask_to_replace(question),
                        },
                        end => {
                            finished = Some(end);
                            break;
                        }
                    }
                }
                // A frame for an older slider position is drawn only until
                // the newer one arrives, and never over it.
                Event::Frame { generation, pixels } => {
                    if shown.picture.as_ref().is_none_or(|(g, _)| generation >= *g) {
                        shown.picture = Some((generation, pixels));
                        shown.picture_dirty = true;
                    }
                }
                Event::Strip { at_ms, rgb } => {
                    shown.strip.insert(at_ms, rgb);
                }
                Event::Loudness(batch) => shown.loudness.extend(batch),
            }
        }
        if let Some(end) = finished {
            break end;
        }
    };
    done.store(true, Ordering::SeqCst);
    drop(screen);
    outcome
}

/// How long one batch of events may take before the screen is drawn
/// again: about 30 frames a second however fast events keep arriving.
const BATCH: std::time::Duration = std::time::Duration::from_millis(30);

/// `first` and whatever else arrives, until the channel is empty or
/// `budget` has gone. Draining until empty is not enough: the loudness
/// reader and the bar's grabs can keep the channel busy for seconds, and
/// the screen would not be drawn in all that time.
fn take_batch(first: Event, rx: &mpsc::Receiver<Event>, budget: std::time::Duration) -> Vec<Event> {
    let until = Instant::now() + budget;
    let mut batch = vec![first];
    while Instant::now() < until {
        match rx.try_recv() {
            Ok(event) => batch.push(event),
            Err(_) => break,
        }
    }
    batch
}

/// What to grab for the picture box: exactly its pixels for half blocks;
/// for kitty and iTerm2 a picture up to 960 wide, which the terminal fits
/// to the box itself.
fn want_for(graphics: Graphics, image: ImageBox, (w, h): (u32, u32)) -> Want {
    let sharp = (u32::from(image.cols) * 8).min(960).min(w).max(2);
    match graphics {
        Graphics::Blocks { .. } => Want::Rgb {
            width: u32::from(image.cols),
            height: u32::from(image.rows) * 2,
        },
        Graphics::Kitty => Want::Rgb {
            width: sharp,
            height: (sharp * h / w.max(1)).max(2),
        },
        Graphics::Iterm => Want::Png { width: sharp },
    }
}

fn redraw(session: &Session, info: &Info, shown: &mut Shown) {
    let size = term::size();
    let mut out = String::new();
    if size != shown.size {
        shown.size = size;
        shown.layout = draw::layout(size.0, size.1, info.size, session.bars().len());
        shown.picture_dirty = true;
        shown.strip_asked.clear();
        if shown.graphics == Graphics::Kitty {
            out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
        }
        out.push_str("\x1b[2J");
    }
    let Some(layout) = shown.layout else {
        out.push_str(
            "\x1b[1;1Hconv trim needs a terminal at least 40 columns wide and 16 rows high; \
             make it bigger, or press q",
        );
        term::put(&out);
        return;
    };
    out.push_str("\x1b[1;1H\x1b[2K");
    out.push_str(&draw::header(info, session, layout.cols, true));
    if let (true, Some(image), Some((_, pixels))) =
        (shown.picture_dirty, layout.image, &shown.picture)
    {
        out.push_str(&picture(shown.graphics, image, pixels));
        shown.picture_dirty = false;
    }
    let bar_width = draw::bar_width(layout.cols);
    let strip: Vec<Option<[u8; 3]>> = draw::column_times(session.view(), bar_width)
        .iter()
        .map(|t| shown.strip.get(t).copied())
        .collect();
    let levels = draw::loudness_columns(&shown.loudness, session.view(), bar_width);
    let truecolor = !matches!(shown.graphics, Graphics::Blocks { truecolor: false });
    for (k, line) in draw::below(session, &strip, &levels, layout.cols, true, truecolor)
        .iter()
        .enumerate()
    {
        out.push_str(&format!("\x1b[{};1H\x1b[2K{line}", layout.below + k as u16));
    }
    term::put(&out);
}

fn picture(graphics: Graphics, image: ImageBox, pixels: &Pixels) -> String {
    match (graphics, pixels) {
        (
            Graphics::Blocks { truecolor },
            Pixels::Rgb {
                width,
                height,
                data,
            },
        ) => draw::blocks(data, *width, *height, truecolor, image.row, image.col),
        (
            Graphics::Kitty,
            Pixels::Rgb {
                width,
                height,
                data,
            },
        ) => draw::kitty(data, *width, *height, image.cols, image.row, image.col),
        (Graphics::Iterm, Pixels::Png(png)) => draw::iterm(png, image.cols, image.row, image.col),
        // A picture grabbed for another way of drawing.
        _ => String::new(),
    }
}

/// Grabs the frame under the slider. Requests arrive faster than frames
/// can be grabbed while a key is held, so only the latest is grabbed.
fn frame_worker(
    source: &Source,
    events: Sender<Event>,
    done: Arc<AtomicBool>,
) -> Sender<(u64, u64, Want)> {
    let (tx, rx) = mpsc::channel::<(u64, u64, Want)>();
    let (ffmpeg, file) = (source.ffmpeg.to_path_buf(), source.file.to_path_buf());
    std::thread::spawn(move || {
        while let Ok(mut request) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                request = newer;
            }
            if done.load(Ordering::SeqCst) {
                break;
            }
            let (generation, at, want) = request;
            // A frame that cannot be read (past the last one) leaves the
            // last picture up.
            if let Ok(pixels) = frames::grab(&ffmpeg, &file, at, want) {
                if events.send(Event::Frame { generation, pixels }).is_err() {
                    break;
                }
            }
        }
    });
    tx
}

/// Grabs one colour for each column of the video bar, in parallel. A newer
/// request (the bars zoomed or moved) makes the rest of an older one moot.
fn strip_worker(
    source: &Source,
    events: Sender<Event>,
    generation: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
) -> Sender<(u64, Vec<u64>)> {
    let (tx, rx) = mpsc::channel::<(u64, Vec<u64>)>();
    let (ffmpeg, file) = (source.ffmpeg.to_path_buf(), source.file.to_path_buf());
    std::thread::spawn(move || {
        while let Ok((asked, times)) = rx.recv() {
            times.par_iter().for_each(|&at| {
                if done.load(Ordering::SeqCst) || generation.load(Ordering::SeqCst) != asked {
                    return;
                }
                let one = Want::Rgb {
                    width: 1,
                    height: 1,
                };
                if let Ok(Pixels::Rgb { data, .. }) = frames::grab(&ffmpeg, &file, at, one) {
                    let _ = events.send(Event::Strip {
                        at_ms: at,
                        rgb: [data[0], data[1], data[2]],
                    });
                }
            });
        }
    });
    tx
}

/// Reads the loudness of the whole file once, sending it as it comes, and
/// stops ffmpeg when the session ends: a long file would otherwise go on
/// being decoded while its clips are cut.
fn loudness_worker(source: &Source, events: Sender<Event>, done: Arc<AtomicBool>) {
    let (ffmpeg, file) = (source.ffmpeg.to_path_buf(), source.file.to_path_buf());
    std::thread::spawn(move || {
        // Sent 5 s of sound at a time: ffmpeg's reads hand over a few
        // buckets each, and one event per read kept the event loop busy.
        const SEND_EVERY: usize = 500;
        let mut pending = Vec::with_capacity(SEND_EVERY);
        // A sound that cannot be read leaves the bar empty; the clips are
        // still cut by the times on screen.
        let _ = frames::loudness(&ffmpeg, &file, &mut |batch| {
            if done.load(Ordering::SeqCst) {
                return ControlFlow::Break(());
            }
            pending.extend_from_slice(batch);
            if pending.len() >= SEND_EVERY
                && events
                    .send(Event::Loudness(std::mem::take(&mut pending)))
                    .is_err()
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        if !pending.is_empty() {
            let _ = events.send(Event::Loudness(pending));
        }
    });
}

/// Where and how the clips are written, so that each one is checked while
/// the screen is still up: a clip found unwritable once the session is
/// over would take every mark with it.
struct Writing<'a> {
    file: &'a Path,
    outdir: Option<&'a Path>,
    target: &'a clips::Target,
    probe: &'a MediaProbe,
    overwrite: bool,
    dry_run: bool,
}

/// Whether the clips can be written as they are.
#[derive(Debug, PartialEq, Eq)]
enum Ready {
    Go,
    /// Not as they are: why, and what to do.
    Refuse(String),
    /// Only over files already there: the question to ask first.
    Ask(String),
}

impl Writing<'_> {
    /// Plans the clip as `conv FILE --start --end` would, so it is
    /// refused for what the flags would refuse. Planning runs nothing.
    fn plan(&self, clip: &session::Clip) -> Result<ConversionPlan, ConvError> {
        let (job, tuning) = clips::job(self.file, self.outdir, clip, self.target);
        plan::build_tuned(
            job.from,
            job.to,
            &job.inputs,
            &job.output,
            Some(self.probe),
            None,
            &tuning,
        )
    }

    fn check(&self, clip: &session::Clip) -> Result<(), ConvError> {
        self.plan(clip).map(|_| ())
    }

    /// Checks the clips' names before `w` leaves the screen: two clips
    /// writing one file would race in the batch, and a file already there
    /// is replaced only when asked.
    fn ready(&self, marked: &[session::Clip]) -> Ready {
        let outputs: Vec<PathBuf> = marked
            .iter()
            .map(|c| clips::job(self.file, self.outdir, c, self.target).0.output)
            .collect();
        let name = |p: &Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let mut seen = std::collections::HashSet::new();
        for output in &outputs {
            if !seen.insert(crate::input::collision_key(output)) {
                return Ready::Refuse(format!(
                    "Two clips would both write {}; press u to drop the last.",
                    name(output)
                ));
            }
        }
        if self.overwrite || self.dry_run {
            return Ready::Go;
        }
        let there: Vec<&PathBuf> = outputs.iter().filter(|o| o.exists()).collect();
        match there.as_slice() {
            [] => Ready::Go,
            [one] => Ready::Ask(format!("{} is already there. Replace it? [y/N]", name(one))),
            many => Ready::Ask(format!(
                "{} clips would replace files already there. Replace them? [y/N]",
                many.len()
            )),
        }
    }
}

/// Writes every clip, after the screen has been given back, as `conv FILE
/// --start --end` would: through the same planning and the same batch,
/// with the subcommand's `-o`. `overwrite` is `-y`, or the answer to the
/// question `w` asked.
fn write(
    cli: &Cli,
    writing: &Writing,
    overwrite: bool,
    marked: &[session::Clip],
) -> Result<i32, ConvError> {
    if writing.dry_run {
        let mut code = 0;
        for clip in marked {
            match writing.plan(clip) {
                Ok(p) => print!("{}", render::plan_human(&p)),
                Err(e) => {
                    eprint!("{}", render::error_human(&e));
                    code = e.code.exit_code();
                }
            }
        }
        return Ok(code);
    }
    if let Some(dir) = writing.outdir {
        std::fs::create_dir_all(dir).map_err(|e| {
            ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("cannot create output directory {}: {e}", dir.display()),
            )
        })?;
    }
    let pairs: Vec<_> = marked
        .iter()
        .map(|c| clips::job(writing.file, writing.outdir, c, writing.target))
        .collect();
    let mut runner = cli.clone();
    runner.overwrite = overwrite;
    runner.outdir = writing.outdir.map(PathBuf::from);
    runner.command = None;
    let (results, code, _) = batch::run_each(pairs, &runner, false);
    let (styled_out, styled_err) = (render::stdout_styled(), render::stderr_styled());
    for r in &results {
        match &r.result {
            Ok(o) => {
                print!("{}", render::conversion_success_human(o, styled_out));
                eprint!("{}", render::conversion_notes_human("", o, &[], styled_err));
            }
            Err(e) => eprint!(
                "{}",
                render::conversion_failure_human(&r.input, r.to.ext(), e, styled_err)
            ),
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The loudness reader and the bar's grabs send events faster than
    /// they can all be handled at times; a batch still ends on time, so
    /// the screen keeps being drawn.
    #[test]
    fn a_batch_of_events_ends_on_time_however_many_are_waiting() {
        let (tx, rx) = mpsc::channel();
        // More waiting than a few milliseconds can take: draining them
        // all, as the loop once did, takes far longer than the budget.
        const WAITING: u64 = 3_000_000;
        for at_ms in 0..WAITING {
            tx.send(Event::Strip { at_ms, rgb: [0; 3] }).unwrap();
        }
        let first = rx.recv().unwrap();
        let started = Instant::now();
        let batch = take_batch(first, &rx, Duration::from_millis(2));
        let took = started.elapsed();
        assert!(batch.len() > 1, "it takes what is waiting");
        assert!(
            (batch.len() as u64) < WAITING,
            "it stops on time, leaving the rest for after the next draw"
        );
        assert!(took < Duration::from_millis(100), "{took:?}");
        // What it left is still there.
        assert!(rx.try_recv().is_ok());
    }

    fn probe() -> MediaProbe {
        MediaProbe {
            video_codec: Some("h264".into()),
            audio_codecs: vec!["aac".into()],
            video_streams: 1,
            width: Some(320),
            height: Some(180),
            frame_rate: Some((30, 1)),
            duration_ms: Some(20_000),
            ..MediaProbe::default()
        }
    }

    fn clip(start_ms: u64, end_ms: u64, keeps: session::Keeps) -> session::Clip {
        session::Clip {
            start_ms,
            end_ms,
            keeps,
        }
    }

    const MP4: clips::Target = clips::Target {
        video: Some(Format::Mp4),
        audio: Format::M4a,
    };

    fn writing<'a>(
        file: &'a Path,
        target: &'a clips::Target,
        probe: &'a MediaProbe,
    ) -> Writing<'a> {
        Writing {
            file,
            outdir: None,
            target,
            probe,
            overwrite: false,
            dry_run: false,
        }
    }

    /// Whatever `conv FILE --start --end` would refuse is refused as the
    /// clip is marked, while the marks are still on screen.
    #[test]
    fn a_clip_is_checked_as_the_flags_would_check_it() {
        let probe = probe();
        let w = writing(Path::new("src.mp4"), &MP4, &probe);
        let e = w
            .check(&clip(2_000, 2_010, session::Keeps::Both))
            .unwrap_err();
        assert!(e.message.contains("shorter than one frame"), "{e}");
        // The sound alone has no frames to be shorter than.
        assert!(w.check(&clip(2_000, 2_010, session::Keeps::Audio)).is_ok());
        assert!(w.check(&clip(2_000, 4_000, session::Keeps::Both)).is_ok());
    }

    #[test]
    fn w_asks_before_replacing_files_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.mp4");
        let probe = probe();
        let mut w = writing(&file, &MP4, &probe);
        let both = [
            clip(2_000, 4_000, session::Keeps::Both),
            clip(2_000, 4_000, session::Keeps::Audio),
        ];
        assert_eq!(w.ready(&both), Ready::Go);
        std::fs::write(dir.path().join("src-2s-4s.mp4"), b"").unwrap();
        assert_eq!(
            w.ready(&both),
            Ready::Ask("src-2s-4s.mp4 is already there. Replace it? [y/N]".to_string())
        );
        std::fs::write(dir.path().join("src-2s-4s.m4a"), b"").unwrap();
        assert_eq!(
            w.ready(&both),
            Ready::Ask(
                "2 clips would replace files already there. Replace them? [y/N]".to_string()
            )
        );
        w.overwrite = true;
        assert_eq!(w.ready(&both), Ready::Go, "-y has said yes");
        w.overwrite = false;
        w.dry_run = true;
        assert_eq!(w.ready(&both), Ready::Go, "a dry run writes nothing");
    }

    /// The batch runs clips in parallel, and two writing one file would race.
    #[test]
    fn w_refuses_two_clips_that_would_write_one_file() {
        let gif = clips::Target {
            video: Some(Format::Gif),
            audio: Format::M4a,
        };
        let probe = probe();
        let w = writing(Path::new("src.mp4"), &gif, &probe);
        let twice = [
            clip(2_000, 4_000, session::Keeps::Both),
            clip(2_000, 4_000, session::Keeps::Video),
        ];
        assert_eq!(
            w.ready(&twice),
            Ready::Refuse(
                "Two clips would both write src-2s-4s.gif; press u to drop the last.".to_string()
            )
        );
    }
}
