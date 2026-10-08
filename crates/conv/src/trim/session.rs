//! What `conv trim` is doing, apart from the screen: the slider, the view,
//! the bars, the marks and the clips, changed one key at a time. Pure: no
//! terminal, no threads, no clock of its own (every key comes with its
//! time), so all of it is tested here.

use std::time::{Duration, Instant};

/// A tap of ←/→ moves this far.
pub const TAP_MS: u64 = 10;
/// Held, the slider moves this many milliseconds for each one held: 2 s a
/// second.
pub const HOLD_SPEED: u64 = 2;
/// A terminal sends no key releases, only the press again, about 30 times
/// a second while a key is held, after a first pause of 250-600 ms. An
/// arrow this soon after the last one in the same direction is a hold.
pub const HOLD_GAP: Duration = Duration::from_millis(150);
/// Zooming in stops at a view this long.
pub const MIN_VIEW_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    Video,
    Audio,
}

/// What a clip keeps: picked by the selected bar when its start is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keeps {
    Both,
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clip {
    pub start_ms: u64,
    pub end_ms: u64,
    pub keeps: Keeps,
}

/// A key, as the session understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    FrameBack,
    FrameForward,
    ZoomIn,
    ZoomOut,
    Up,
    Down,
    Enter,
    Cut,
    Undo,
    Escape,
    Write,
    Quit,
    Yes,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Write,
    /// Write, replacing the files already there: the answer to
    /// `ask_to_replace`.
    Replace,
    Quit,
}

/// A question on the message line, which the next key answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    Quit,
    Replace,
}

#[derive(Debug, Clone)]
pub struct Session {
    duration_ms: u64,
    frame_ms: u64,
    bars: Vec<Bar>,
    slider_ms: u64,
    view_start: u64,
    view_len: u64,
    highlighted: usize,
    selected: Option<Bar>,
    open: Option<(u64, Keeps)>,
    clips: Vec<Clip>,
    message: Option<String>,
    asking: Option<Ask>,
    picture_alone: bool,
    last_arrow: Option<(i8, Instant)>,
    generation: u64,
}

impl Session {
    /// A session over a file this long. `frame_ms` is one frame's length,
    /// for `,`/`.`; 33 ms (30 fps) when unknown.
    pub fn new(
        duration_ms: u64,
        frame_ms: Option<u64>,
        has_video: bool,
        has_audio: bool,
    ) -> Session {
        let bars = [(has_video, Bar::Video), (has_audio, Bar::Audio)]
            .into_iter()
            .filter_map(|(has, bar)| has.then_some(bar))
            .collect();
        Session {
            duration_ms,
            frame_ms: frame_ms.filter(|&f| f > 0).unwrap_or(33),
            bars,
            slider_ms: 0,
            view_start: 0,
            view_len: duration_ms,
            highlighted: 0,
            selected: None,
            open: None,
            clips: Vec::new(),
            message: None,
            asking: None,
            picture_alone: false,
            last_arrow: None,
            generation: 0,
        }
    }

    /// For a video target with no sound in it, a GIF: a clip with the
    /// picture keeps the picture alone, whether or not the video bar is
    /// selected, so one clip can't be marked twice as two.
    pub fn picture_alone(mut self) -> Session {
        self.picture_alone = true;
        self
    }

    /// Drops the clip just marked, which can't be written, saying why.
    pub fn refuse_last(&mut self, why: &str) {
        self.clips.pop();
        self.message = Some(why.to_string());
    }

    /// Asks before writing over files already there; `y` gives
    /// `Outcome::Replace`, any other key keeps the session.
    pub fn ask_to_replace(&mut self, question: String) {
        self.asking = Some(Ask::Replace);
        self.message = Some(question);
    }

    /// Shows a message until the next key.
    pub fn tell(&mut self, message: String) {
        self.message = Some(message);
    }

    pub fn input(&mut self, input: Input, now: Instant) -> Outcome {
        // A question takes the next key, whatever it is.
        if let Some(ask) = self.asking.take() {
            self.message = None;
            return match (ask, input) {
                (Ask::Quit, Input::Yes) => Outcome::Quit,
                (Ask::Replace, Input::Yes) => Outcome::Replace,
                _ => Outcome::Continue,
            };
        }
        self.message = None;
        if !matches!(input, Input::Left | Input::Right) {
            self.last_arrow = None;
        }
        match input {
            Input::Left => self.arrow(-1, now),
            Input::Right => self.arrow(1, now),
            Input::PageUp => self.move_by(-(self.view_len as i64 / 10)),
            Input::PageDown => self.move_by(self.view_len as i64 / 10),
            Input::Home => self.move_to(0),
            Input::End => self.move_to(self.duration_ms),
            Input::FrameBack => self.move_by(-(self.frame_ms as i64)),
            Input::FrameForward => self.move_by(self.frame_ms as i64),
            Input::ZoomIn => self.zoom_to(self.view_len / 2),
            Input::ZoomOut => self.zoom_to(self.view_len.saturating_mul(2)),
            Input::Up => self.highlighted = self.highlighted.saturating_sub(1),
            Input::Down => {
                self.highlighted = (self.highlighted + 1).min(self.bars.len().saturating_sub(1))
            }
            Input::Enter => self.select(),
            Input::Cut => self.cut(),
            Input::Undo => {
                if self.open.take().is_none() {
                    self.clips.pop();
                }
            }
            Input::Escape => self.open = None,
            Input::Write => return self.write(),
            Input::Quit => return self.quit(),
            Input::Yes | Input::Other => {}
        }
        Outcome::Continue
    }

    /// A tap moves `TAP_MS`; a hold moves `HOLD_SPEED` times the time since
    /// the last arrow, so the speed does not depend on the keyboard's
    /// repeat rate. Keys that arrive together still move a tap each.
    fn arrow(&mut self, dir: i8, now: Instant) {
        let step = match self.last_arrow {
            Some((d, then)) if d == dir && now.saturating_duration_since(then) <= HOLD_GAP => {
                let held = now.saturating_duration_since(then).as_millis() as u64;
                (held * HOLD_SPEED).max(TAP_MS)
            }
            _ => TAP_MS,
        };
        self.last_arrow = Some((dir, now));
        self.move_by(i64::from(dir) * step as i64);
    }

    fn move_by(&mut self, delta: i64) {
        let to = (self.slider_ms as i64).saturating_add(delta).max(0) as u64;
        self.move_to(to);
    }

    fn move_to(&mut self, ms: u64) {
        let ms = ms.min(self.duration_ms);
        if ms == self.slider_ms {
            return;
        }
        self.slider_ms = ms;
        self.generation += 1;
        // The view follows the slider out of either side.
        if ms < self.view_start {
            self.view_start = ms;
        } else if ms > self.view_start + self.view_len {
            self.view_start = ms - self.view_len;
        }
    }

    fn zoom_to(&mut self, len: u64) {
        let len = len
            .max(MIN_VIEW_MS.min(self.duration_ms))
            .min(self.duration_ms);
        self.view_len = len;
        self.view_start = self
            .slider_ms
            .saturating_sub(len / 2)
            .min(self.duration_ms - len);
    }

    fn select(&mut self) {
        // With one bar there is nothing to choose between.
        if self.bars.len() < 2 {
            return;
        }
        let bar = self.bars[self.highlighted];
        self.selected = if self.selected == Some(bar) {
            None
        } else {
            Some(bar)
        };
    }

    fn keeps(&self) -> Keeps {
        match self.selected {
            None if self.picture_alone && self.bars.contains(&Bar::Video) => Keeps::Video,
            None => Keeps::Both,
            Some(Bar::Video) => Keeps::Video,
            Some(Bar::Audio) => Keeps::Audio,
        }
    }

    fn cut(&mut self) {
        let Some((start, keeps)) = self.open.take() else {
            self.open = Some((self.slider_ms, self.keeps()));
            return;
        };
        let (start_ms, end_ms) = if start <= self.slider_ms {
            (start, self.slider_ms)
        } else {
            (self.slider_ms, start)
        };
        if start_ms == end_ms {
            self.open = Some((start, keeps));
            self.message = Some(
                "A clip needs a length: move the slider to its end, then press c.".to_string(),
            );
            return;
        }
        let clip = Clip {
            start_ms,
            end_ms,
            keeps,
        };
        if self.clips.contains(&clip) {
            self.message = Some("That clip is already marked.".to_string());
            return;
        }
        self.clips.push(clip);
    }

    fn write(&mut self) -> Outcome {
        if self.open.is_some() {
            self.message = Some(
                "Finish the open clip first: press c at its end, or Esc to drop it.".to_string(),
            );
            return Outcome::Continue;
        }
        if self.clips.is_empty() {
            self.message =
                Some("No clips yet: press c at a clip's start and again at its end.".to_string());
            return Outcome::Continue;
        }
        Outcome::Write
    }

    fn quit(&mut self) -> Outcome {
        let question = match (self.clips.len(), self.open) {
            (0, None) => return Outcome::Quit,
            (0, Some(_)) => "Quit and drop the open clip? [y/N]".to_string(),
            (1, _) => "Quit without writing 1 clip? [y/N]".to_string(),
            (n, _) => format!("Quit without writing {n} clips? [y/N]"),
        };
        self.asking = Some(Ask::Quit);
        self.message = Some(question);
        Outcome::Continue
    }

    pub fn slider_ms(&self) -> u64 {
        self.slider_ms
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// The part of the file the bars show: its start and length.
    pub fn view(&self) -> (u64, u64) {
        (self.view_start, self.view_len)
    }

    /// The bars there are, top to bottom.
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    pub fn highlighted(&self) -> Option<Bar> {
        self.bars.get(self.highlighted).copied()
    }

    pub fn selected(&self) -> Option<Bar> {
        self.selected
    }

    pub fn open_mark(&self) -> Option<(u64, Keeps)> {
        self.open
    }

    pub fn clips(&self) -> &[Clip] {
        &self.clips
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Bumps every time the slider moves, so a frame grabbed for an older
    /// position is never drawn over a newer one.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(secs: u64) -> Session {
        Session::new(secs * 1000, Some(33), true, true)
    }

    /// Feeds inputs `gap_ms` apart, starting at `t0`; returns the last
    /// outcome and the time after the last input.
    fn feed(s: &mut Session, t0: Instant, gap_ms: u64, inputs: &[Input]) -> (Outcome, Instant) {
        let mut t = t0;
        let mut out = Outcome::Continue;
        for &i in inputs {
            t += Duration::from_millis(gap_ms);
            out = s.input(i, t);
        }
        (out, t)
    }

    #[test]
    fn a_tap_moves_ten_milliseconds() {
        let mut s = session(60);
        let t = Instant::now();
        s.input(Input::Right, t);
        assert_eq!(s.slider_ms(), 10);
        s.input(Input::Right, t + Duration::from_millis(500));
        assert_eq!(s.slider_ms(), 20, "half a second later is another tap");
        s.input(Input::Left, t + Duration::from_millis(1000));
        assert_eq!(s.slider_ms(), 10);
    }

    #[test]
    fn holding_moves_two_seconds_a_second_whatever_the_repeat_rate() {
        for gap in [25, 33, 50, 100] {
            let mut s = session(600);
            let repeats = 1000 / gap;
            feed(
                &mut s,
                Instant::now(),
                gap,
                &vec![Input::Right; repeats as usize + 1],
            );
            let moved = s.slider_ms();
            assert!(
                (1_900..=2_100).contains(&moved),
                "repeat every {gap} ms moved {moved} ms in a second"
            );
        }
    }

    #[test]
    fn turning_round_or_pausing_starts_a_new_tap() {
        let mut s = session(60);
        let (_, t) = feed(&mut s, Instant::now(), 33, &[Input::Right; 31]);
        let after_hold = s.slider_ms();
        s.input(Input::Left, t + Duration::from_millis(33));
        assert_eq!(s.slider_ms(), after_hold - TAP_MS);
        s.input(Input::Left, t + Duration::from_millis(600));
        assert_eq!(s.slider_ms(), after_hold - 2 * TAP_MS);
    }

    #[test]
    fn keys_arriving_together_move_a_tap_each() {
        let mut s = session(60);
        let t = Instant::now();
        for _ in 0..50 {
            s.input(Input::Right, t);
        }
        assert_eq!(s.slider_ms(), 500);
    }

    #[test]
    fn the_slider_stays_in_the_file() {
        let mut s = session(10);
        let t = Instant::now();
        s.input(Input::Left, t);
        assert_eq!(s.slider_ms(), 0);
        s.input(Input::End, t);
        assert_eq!(s.slider_ms(), 10_000);
        s.input(Input::Right, t + Duration::from_millis(500));
        assert_eq!(s.slider_ms(), 10_000);
        s.input(Input::Home, t);
        assert_eq!(s.slider_ms(), 0);
    }

    #[test]
    fn page_keys_move_a_tenth_of_the_view_and_frame_keys_a_frame() {
        let mut s = session(100);
        let t = Instant::now();
        s.input(Input::PageDown, t);
        assert_eq!(s.slider_ms(), 10_000);
        s.input(Input::FrameForward, t);
        assert_eq!(s.slider_ms(), 10_033);
        s.input(Input::FrameBack, t);
        s.input(Input::PageUp, t);
        assert_eq!(s.slider_ms(), 0);
        let mut unknown_rate = Session::new(100_000, None, true, false);
        unknown_rate.input(Input::FrameForward, t);
        assert_eq!(unknown_rate.slider_ms(), 33);
    }

    #[test]
    fn zoom_halves_around_the_slider_and_stops_at_two_seconds() {
        let mut s = session(600);
        let t = Instant::now();
        assert_eq!(s.view(), (0, 600_000));
        s.input(Input::PageDown, t); // 60 s
        s.input(Input::ZoomIn, t);
        assert_eq!(s.view(), (0, 300_000));
        s.input(Input::ZoomIn, t);
        assert_eq!(s.view(), (0, 150_000));
        s.input(Input::ZoomIn, t);
        assert_eq!(s.view(), (22_500, 75_000), "centred on 60 s");
        for _ in 0..20 {
            s.input(Input::ZoomIn, t);
        }
        assert_eq!(s.view().1, MIN_VIEW_MS);
        let (start, len) = s.view();
        assert!(start <= s.slider_ms() && s.slider_ms() <= start + len);
        for _ in 0..20 {
            s.input(Input::ZoomOut, t);
        }
        assert_eq!(s.view(), (0, 600_000));
    }

    #[test]
    fn the_view_follows_the_slider() {
        let mut s = session(600);
        let t = Instant::now();
        for _ in 0..8 {
            s.input(Input::ZoomIn, t); // ~2.3 s around 0
        }
        let (_, len) = s.view();
        for _ in 0..5 {
            s.input(Input::PageDown, t);
        }
        let (start, _) = s.view();
        assert!(
            start <= s.slider_ms() && s.slider_ms() <= start + len,
            "{:?} {}",
            s.view(),
            s.slider_ms()
        );
    }

    #[test]
    fn up_down_highlight_a_bar_and_enter_selects_it() {
        let mut s = session(60);
        let t = Instant::now();
        assert_eq!(s.bars(), [Bar::Video, Bar::Audio]);
        assert_eq!(s.highlighted(), Some(Bar::Video));
        assert_eq!(s.selected(), None);
        s.input(Input::Down, t);
        assert_eq!(s.highlighted(), Some(Bar::Audio));
        s.input(Input::Down, t);
        assert_eq!(
            s.highlighted(),
            Some(Bar::Audio),
            "it stops at the last bar"
        );
        s.input(Input::Enter, t);
        assert_eq!(s.selected(), Some(Bar::Audio));
        s.input(Input::Enter, t);
        assert_eq!(s.selected(), None, "Enter on the selected bar deselects it");
        s.input(Input::Up, t);
        s.input(Input::Enter, t);
        assert_eq!(s.selected(), Some(Bar::Video));
    }

    #[test]
    fn a_file_with_one_kind_of_stream_has_one_bar_and_nothing_to_select() {
        let t = Instant::now();
        let mut audio = Session::new(60_000, None, false, true);
        assert_eq!(audio.bars(), [Bar::Audio]);
        audio.input(Input::Enter, t);
        assert_eq!(audio.selected(), None);
        let video = Session::new(60_000, Some(40), true, false);
        assert_eq!(video.bars(), [Bar::Video]);
    }

    #[test]
    fn c_marks_a_start_then_an_end_with_what_the_selection_keeps() {
        let mut s = session(60);
        let t = Instant::now();
        s.input(Input::PageDown, t); // 6 s
        s.input(Input::Cut, t);
        assert_eq!(s.open_mark(), Some((6_000, Keeps::Both)));
        s.input(Input::PageDown, t); // 12 s
        s.input(Input::Cut, t);
        assert_eq!(s.open_mark(), None);
        assert_eq!(
            s.clips(),
            [Clip {
                start_ms: 6_000,
                end_ms: 12_000,
                keeps: Keeps::Both
            }]
        );
        s.input(Input::Down, t);
        s.input(Input::Enter, t); // audio
        s.input(Input::Cut, t);
        s.input(Input::PageUp, t); // back to 6 s: the ends are ordered
        s.input(Input::Cut, t);
        assert_eq!(
            s.clips()[1],
            Clip {
                start_ms: 6_000,
                end_ms: 12_000,
                keeps: Keeps::Audio
            }
        );
    }

    #[test]
    fn a_clip_needs_a_length_and_is_marked_once() {
        let mut s = session(60);
        let t = Instant::now();
        s.input(Input::Cut, t);
        s.input(Input::Cut, t);
        assert!(s.clips().is_empty());
        assert_eq!(s.open_mark(), Some((0, Keeps::Both)), "still open");
        assert!(s.message().unwrap().contains("needs a length"));
        s.input(Input::PageDown, t);
        s.input(Input::Cut, t);
        s.input(Input::Cut, t);
        s.input(Input::PageUp, t);
        s.input(Input::Cut, t);
        assert_eq!(s.clips().len(), 1);
        assert_eq!(s.message(), Some("That clip is already marked."));
    }

    #[test]
    fn escape_drops_the_open_mark_and_u_undoes() {
        let mut s = session(60);
        let t = Instant::now();
        s.input(Input::Cut, t);
        s.input(Input::Escape, t);
        assert_eq!(s.open_mark(), None);
        s.input(Input::Cut, t);
        s.input(Input::PageDown, t);
        s.input(Input::Cut, t);
        s.input(Input::Cut, t);
        s.input(Input::Undo, t);
        assert_eq!(s.open_mark(), None, "u takes back the open mark first");
        assert_eq!(s.clips().len(), 1);
        s.input(Input::Undo, t);
        assert!(s.clips().is_empty(), "then the last clip");
    }

    #[test]
    fn w_writes_only_finished_clips() {
        let mut s = session(60);
        let t = Instant::now();
        assert_eq!(s.input(Input::Write, t), Outcome::Continue);
        assert!(s.message().unwrap().contains("No clips yet"));
        s.input(Input::Cut, t);
        s.input(Input::PageDown, t);
        s.input(Input::Cut, t);
        s.input(Input::Cut, t);
        assert_eq!(s.input(Input::Write, t), Outcome::Continue);
        assert!(s.message().unwrap().contains("Finish the open clip"));
        s.input(Input::Escape, t);
        assert_eq!(s.input(Input::Write, t), Outcome::Write);
    }

    #[test]
    fn q_quits_at_once_with_nothing_to_lose_and_asks_otherwise() {
        let t = Instant::now();
        let mut empty = session(60);
        assert_eq!(empty.input(Input::Quit, t), Outcome::Quit);

        let mut s = session(60);
        s.input(Input::Cut, t);
        s.input(Input::PageDown, t);
        s.input(Input::Cut, t);
        assert_eq!(s.input(Input::Quit, t), Outcome::Continue);
        assert_eq!(s.message(), Some("Quit without writing 1 clip? [y/N]"));
        assert_eq!(s.input(Input::Other, t), Outcome::Continue);
        assert_eq!(s.message(), None, "anything but y keeps the session");
        assert_eq!(s.clips().len(), 1);
        s.input(Input::Quit, t);
        assert_eq!(s.input(Input::Yes, t), Outcome::Quit);

        let mut open = session(60);
        open.input(Input::Cut, t);
        open.input(Input::Quit, t);
        assert_eq!(open.message(), Some("Quit and drop the open clip? [y/N]"));
    }

    #[test]
    fn every_move_of_the_slider_bumps_the_generation() {
        let mut s = session(60);
        let t = Instant::now();
        let g = s.generation();
        s.input(Input::Right, t);
        assert!(s.generation() > g);
        let g = s.generation();
        s.input(Input::Left, t + Duration::from_millis(400));
        s.input(Input::Left, t + Duration::from_millis(800)); // at 0 already
        assert_eq!(
            s.generation(),
            g + 1,
            "a key that moves nothing bumps nothing"
        );
        let g = s.generation();
        s.input(Input::Cut, t);
        assert_eq!(s.generation(), g);
    }

    /// Marks a clip from 0 to the next tenth of the view.
    fn mark(s: &mut Session, t: Instant) {
        s.input(Input::Home, t);
        s.input(Input::Cut, t);
        s.input(Input::PageDown, t);
        s.input(Input::Cut, t);
    }

    #[test]
    fn a_clip_that_cannot_be_written_is_dropped_with_the_reason() {
        let mut s = session(60);
        let t = Instant::now();
        mark(&mut s, t);
        s.refuse_last("Not marked: too short.");
        assert!(s.clips().is_empty());
        assert_eq!(s.message(), Some("Not marked: too short."));
        assert_eq!(s.open_mark(), None);
    }

    #[test]
    fn the_replace_question_takes_the_next_key() {
        let mut s = session(60);
        let t = Instant::now();
        mark(&mut s, t);
        assert_eq!(s.input(Input::Write, t), Outcome::Write);
        s.ask_to_replace("Replace it? [y/N]".to_string());
        assert_eq!(s.message(), Some("Replace it? [y/N]"));
        assert_eq!(s.input(Input::Quit, t), Outcome::Continue);
        assert_eq!(s.message(), None, "anything but y keeps the session");
        assert_eq!(s.clips().len(), 1);
        s.input(Input::Write, t);
        s.ask_to_replace("Replace it? [y/N]".to_string());
        assert_eq!(s.input(Input::Yes, t), Outcome::Replace);
    }

    #[test]
    fn a_message_from_outside_shows_until_the_next_key() {
        let mut s = session(60);
        let t = Instant::now();
        s.tell("Two clips would both write a.gif.".to_string());
        assert_eq!(s.message(), Some("Two clips would both write a.gif."));
        s.input(Input::Right, t);
        assert_eq!(s.message(), None);
    }

    /// A GIF holds no sound, so the whole clip and the video bar's clip of
    /// one range are the same file, and marking both is marking it twice.
    #[test]
    fn with_the_picture_alone_a_clip_with_the_picture_keeps_only_that() {
        let mut s = session(60).picture_alone();
        let t = Instant::now();
        mark(&mut s, t);
        assert_eq!(s.clips()[0].keeps, Keeps::Video);
        s.input(Input::Up, t);
        s.input(Input::Enter, t); // the video bar
        mark(&mut s, t);
        assert_eq!(s.clips().len(), 1);
        assert_eq!(s.message(), Some("That clip is already marked."));
        s.input(Input::Down, t);
        s.input(Input::Enter, t); // the audio bar: sound alone still
        mark(&mut s, t);
        assert_eq!(s.clips()[1].keeps, Keeps::Audio);

        let mut sound = Session::new(60_000, None, false, true).picture_alone();
        mark(&mut sound, t);
        assert_eq!(sound.clips()[0].keeps, Keeps::Both, "no picture to keep");
    }
}
