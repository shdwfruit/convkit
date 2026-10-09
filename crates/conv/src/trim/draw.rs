//! Drawing `conv trim`: the picture, the bars and the lines of text, as
//! strings of text and escape codes. Pure, so every piece is tested here;
//! `term.rs` writes them.

use base64::Engine;
use convkit_core::frames::{dbfs, BUCKET_MS};
use convkit_core::trim::format_time;

use super::session::{Bar, Keeps, Session};

/// How the picture is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    /// Half blocks, which every terminal draws. Truecolor where the
    /// terminal says it has it, the 256-colour palette otherwise.
    Blocks { truecolor: bool },
    /// Sextants: three times the pixels of half blocks, for terminals that
    /// draw the characters themselves rather than from a font.
    Sextants { truecolor: bool },
    /// kitty's graphics protocol (kitty, Ghostty, WezTerm).
    Kitty,
    /// iTerm2's inline images (iTerm2, WezTerm).
    Iterm,
}

/// Where the picture goes, in cells, 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBox {
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub image: Option<ImageBox>,
    /// The first row of the text under the picture.
    pub below: u16,
    pub cols: u16,
}

/// What the header says about the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    pub size: Option<(u32, u32)>,
    pub fps: Option<String>,
}

fn goto(row: u16, col: u16) -> String {
    format!("\x1b[{row};{col}H")
}

/// The SGR parameters for text in colour `c`.
fn fg_params(c: [u8; 3], truecolor: bool) -> String {
    if truecolor {
        format!("38;2;{};{};{}", c[0], c[1], c[2])
    } else {
        format!("38;5;{}", ansi256(c[0], c[1], c[2]))
    }
}

fn fg(c: [u8; 3], truecolor: bool) -> String {
    format!("\x1b[{}m", fg_params(c, truecolor))
}

fn bg(c: Option<[u8; 3]>, truecolor: bool) -> String {
    match c {
        // The default background, for the half of a cell with no pixel.
        None => "\x1b[49m".to_string(),
        Some(c) if truecolor => format!("\x1b[48;2;{};{};{}m", c[0], c[1], c[2]),
        Some(c) => format!("\x1b[48;5;{}m", ansi256(c[0], c[1], c[2])),
    }
}

/// The picture in half blocks: each `▀` is two pixels, the top one in the
/// text colour and the bottom one in the background, so a cell (about
/// twice as tall as it is wide) holds two square pixels. Every line is
/// placed with its own cursor move, so nothing else on the screen is
/// touched.
pub fn blocks(px: &[u8], w: u32, h: u32, truecolor: bool, row: u16, col: u16) -> String {
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 3) as usize;
        [px[i], px[i + 1], px[i + 2]]
    };
    let mut s = String::new();
    for pair in 0..h.div_ceil(2) {
        s.push_str(&goto(row + pair as u16, col));
        let mut last = None;
        for x in 0..w {
            let top = at(x, pair * 2);
            let bottom = (pair * 2 + 1 < h).then(|| at(x, pair * 2 + 1));
            if last != Some((top, bottom)) {
                s.push_str(&fg(top, truecolor));
                s.push_str(&bg(bottom, truecolor));
                last = Some((top, bottom));
            }
            s.push('▀');
        }
        s.push_str("\x1b[0m");
    }
    s
}

/// The parts of a sextant cell, one bit each: top left, top right, middle
/// left, middle right, bottom left, bottom right, as (column, row).
const SPOTS: [(u32, u32); 6] = [(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)];

/// The character that fills the parts of a cell set in `mask` with the
/// text colour. Unicode has U+1FB00 on for every mask but four that older
/// characters already draw: empty, full, and the left and right halves.
pub fn sextant(mask: u8) -> char {
    match mask {
        0 => ' ',
        63 => '█',
        21 => '▌',
        42 => '▐',
        m => {
            let skipped = u32::from(m > 21) + u32::from(m > 42);
            char::from_u32(0x1FB00 + u32::from(m) - 1 - skipped).expect("a sextant")
        }
    }
}

/// Splits a cell's six pixels into the two colours that fit them best:
/// every split is tried, each side coloured with the mean of its pixels,
/// and the one with the least squared error wins. A split and its inverse
/// are the same, so only masks with the first part set are tried, the full
/// cell first so that a cell of one colour is a plain block.
pub fn fit_cell(six: [[u8; 3]; 6]) -> (u8, [u8; 3], [u8; 3]) {
    let mean = |mask: u8, on: bool| {
        let mut sum = [0u32; 3];
        let mut n = 0;
        for (i, p) in six.iter().enumerate() {
            if (mask >> i & 1 == 1) == on {
                for c in 0..3 {
                    sum[c] += u32::from(p[c]);
                }
                n += 1;
            }
        }
        (n > 0).then(|| sum.map(|s| (s / n) as u8))
    };
    let mut best = (u32::MAX, 63, [0; 3], [0; 3]);
    for mask in (1..=63u8).rev().step_by(2) {
        let fg = mean(mask, true).expect("the first part is set");
        let bg = mean(mask, false).unwrap_or(fg);
        let error: u32 = six
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let c = if mask >> i & 1 == 1 { fg } else { bg };
                (0..3)
                    .map(|k| (i32::from(p[k]) - i32::from(c[k])).pow(2) as u32)
                    .sum::<u32>()
            })
            .sum();
        if error < best.0 {
            best = (error, mask, fg, bg);
        }
    }
    (best.1, best.2, best.3)
}

/// The picture in sextants: each cell is 2x3 pixels in two colours, the
/// best fit of the six. `w` and `h` are normally twice and three times the
/// box's cells; a cell past the edge repeats the last row or column.
pub fn sextants(px: &[u8], w: u32, h: u32, truecolor: bool, row: u16, col: u16) -> String {
    let at = |x: u32, y: u32| {
        let i = ((y.min(h - 1) * w + x.min(w - 1)) * 3) as usize;
        [px[i], px[i + 1], px[i + 2]]
    };
    let mut s = String::new();
    for cy in 0..h.div_ceil(3) {
        s.push_str(&goto(row + cy as u16, col));
        let mut last = None;
        for cx in 0..w.div_ceil(2) {
            let six = SPOTS.map(|(dx, dy)| at(cx * 2 + dx, cy * 3 + dy));
            let (mask, top, under) = fit_cell(six);
            if last != Some((top, under)) {
                s.push_str(&fg(top, truecolor));
                s.push_str(&bg(Some(under), truecolor));
                last = Some((top, under));
            }
            s.push(sextant(mask));
        }
        s.push_str("\x1b[0m");
    }
    s
}

/// The xterm 256-colour index nearest an RGB colour: the 6x6x6 cube or the
/// grey ramp, whichever is closer.
pub fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        (0..6)
            .min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(v)).abs())
            .expect("six levels")
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let dist = |c: [u8; 3]| {
        [r, g, b]
            .iter()
            .zip(c)
            .map(|(&a, b)| (i32::from(a) - i32::from(b)).pow(2))
            .sum::<i32>()
    };
    let cube = dist([LEVELS[ri], LEVELS[gi], LEVELS[bi]]);
    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let step = (mean.saturating_sub(8) / 10).min(23) as u8;
    let grey_value = 8 + 10 * step;
    if dist([grey_value; 3]) < cube {
        232 + step
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

/// kitty's largest chunk of base64 per escape sequence.
const KITTY_CHUNK: usize = 4096;

/// The picture through kitty's graphics protocol: earlier pictures are
/// deleted, then this one's RGB is sent in base64 chunks and shown at the
/// cursor, `c` cells wide so the terminal scales it and keeps its shape.
/// `q=2` asks the terminal not to reply: a reply would arrive on stdin and
/// be read as keys.
pub fn kitty(px: &[u8], w: u32, h: u32, cols: u16, row: u16, col: u16) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(px);
    let mut s = String::from("\x1b_Ga=d,d=A,q=2\x1b\\");
    s.push_str(&goto(row, col));
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(KITTY_CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let chunk = std::str::from_utf8(chunk).expect("base64 is ascii");
        if i == 0 {
            s.push_str(&format!(
                "\x1b_Ga=T,f=24,s={w},v={h},c={cols},q=2,m={more};{chunk}\x1b\\"
            ));
        } else {
            s.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    s
}

/// The picture as an iTerm2 inline image, `cols` cells wide with its shape
/// kept. iTerm2 takes an image file, not pixels, hence the PNG.
pub fn iterm(png: &[u8], cols: u16, row: u16, col: u16) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "{}\x1b]1337;File=inline=1;size={};width={cols};preserveAspectRatio=1:{data}\x07",
        goto(row, col),
        png.len()
    )
}

/// Lines of text under the picture: the time, one per bar, the axis, the
/// clips, a message and two lines of keys.
fn text_rows(bars: usize) -> u16 {
    6 + bars as u16
}

/// The smallest terminal `layout` draws in, columns then rows: the header,
/// three rows of picture when there is one, and the text.
fn least_size(aspect: Option<(u32, u32)>, bars: usize) -> (u16, u16) {
    let least_picture = if aspect.is_some() { 3 } else { 0 };
    (40, 1 + least_picture + text_rows(bars))
}

/// What a terminal too small for `layout` shows: the size it needs, and a
/// question waiting for its answer, which would otherwise be asked where it
/// can't be seen.
pub fn too_small(aspect: Option<(u32, u32)>, bars: usize, message: Option<&str>) -> String {
    let (cols, rows) = least_size(aspect.filter(|&(w, h)| w > 0 && h > 0), bars);
    let mut text = format!(
        "conv trim needs a terminal at least {cols} columns wide and {rows} rows high; \
         make it bigger, or press q"
    );
    if let Some(message) = message {
        text.push_str("\r\n");
        text.push_str(message);
    }
    text
}

/// Where everything goes in a terminal `cols` by `rows`: the header on the
/// first row, the picture as large as the text under it allows with its
/// shape kept, then the text. `None` when the terminal is too small to be
/// of use.
pub fn layout(cols: u16, rows: u16, aspect: Option<(u32, u32)>, bars: usize) -> Option<Layout> {
    let aspect = aspect.filter(|&(w, h)| w > 0 && h > 0);
    let text = text_rows(bars);
    let (least_cols, least_rows) = least_size(aspect, bars);
    if cols < least_cols || rows < least_rows {
        return None;
    }
    let Some((w, h)) = aspect else {
        return Some(Layout {
            image: None,
            below: 2,
            cols,
        });
    };
    let room = u32::from(rows - 1 - text);
    // A cell is about twice as tall as it is wide: a picture `c` cells
    // wide is `c * h / w / 2` rows high.
    let fit_cols = (room * 2 * w / h).clamp(1, u32::from(cols)) as u16;
    let fit_rows = (u32::from(fit_cols) * h).div_ceil(2 * w).clamp(1, room) as u16;
    Some(Layout {
        image: Some(ImageBox {
            row: 2,
            col: (cols - fit_cols) / 2 + 1,
            cols: fit_cols,
            rows: fit_rows,
        }),
        below: 2 + fit_rows,
        cols,
    })
}

/// The loudness of each of `width` columns over the view, in dBFS: the RMS
/// of the 10 ms buckets the column spans. `None` for a column none of
/// whose buckets have been read yet.
pub fn loudness_columns(buckets: &[f32], view: (u64, u64), width: usize) -> Vec<Option<f32>> {
    let (start, len) = view;
    (0..width as u64)
        .map(|i| {
            let a = start + len * i / width as u64;
            let b = start + len * (i + 1) / width as u64;
            let first = (a / BUCKET_MS) as usize;
            let last = ((b / BUCKET_MS) as usize).max(first + 1).min(buckets.len());
            if first >= last {
                return None;
            }
            let span = &buckets[first..last];
            let mean_square = span.iter().map(|r| r * r).sum::<f32>() / span.len() as f32;
            Some(dbfs(mean_square.sqrt()))
        })
        .collect()
}

/// The time each of `width` columns over the view starts at, snapped to a
/// grid one column wide, so moving the view along reuses the colours
/// already grabbed for the video bar instead of asking for every column
/// again.
pub fn column_times(view: (u64, u64), width: usize) -> Vec<u64> {
    let (start, len) = view;
    let step = len.div_ceil(width.max(1) as u64).max(1);
    (0..width as u64)
        .map(|i| (start + len * i / width as u64) / step * step)
        .collect()
}

/// How many columns the bars take in a terminal `cols` wide.
pub fn bar_width(cols: u16) -> usize {
    usize::from(cols).saturating_sub(LABEL).max(1)
}

/// Cuts a line of plain text to `width` characters.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(width.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn paint(text: &str, sgr: &str, styled: bool) -> String {
    if styled {
        format!("\x1b[{sgr}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The first line: the file, its length, its picture size and rate, and
/// the part of it the bars show when zoomed in.
pub fn header(info: &Info, session: &Session, width: u16, styled: bool) -> String {
    let mut parts = vec![format_time(session.duration_ms())];
    if let Some((w, h)) = info.size {
        parts.push(format!("{w}x{h}"));
    }
    if let Some(fps) = &info.fps {
        parts.push(format!("{fps} fps"));
    }
    let (start, len) = session.view();
    if len < session.duration_ms() {
        parts.push(format!(
            "showing {}-{}",
            format_time(start),
            format_time(start + len)
        ));
    }
    let rest = format!("  {}", parts.join("  "));
    let width = usize::from(width);
    let name = fit(
        &info.name,
        width.saturating_sub(rest.chars().count()).max(8),
    );
    let line = fit(&format!("{name}{rest}"), width);
    match line.strip_prefix(&name) {
        Some(tail) if styled => format!("{}{tail}", paint(&name, "1", true)),
        _ => line,
    }
}

/// The label column before each bar: `>` marks the selected bar.
const LABEL: usize = 8;
const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Quieter than this, a column of the loudness bar is drawn as quiet.
const QUIET_DBFS: f32 = -50.0;

fn keeps_bar(keeps: Keeps, bar: Bar) -> bool {
    matches!(
        (keeps, bar),
        (Keeps::Both, _) | (Keeps::Video, Bar::Video) | (Keeps::Audio, Bar::Audio)
    )
}

fn clip_words(start: u64, end: u64, keeps: Keeps) -> String {
    let kind = match keeps {
        Keeps::Both => "",
        Keeps::Video => " silent",
        Keeps::Audio => " audio",
    };
    format!("{}-{}{kind}", format_time(start), format_time(end))
}

/// Everything under the picture, one string a line: the slider's time, a
/// line per bar, the axis, the clips, the message and the keys. A clip is
/// bracketed on the bars it keeps; the slider is `│` on every bar.
pub fn below(
    session: &Session,
    strip: &[Option<[u8; 3]>],
    levels: &[Option<f32>],
    width: u16,
    styled: bool,
    truecolor: bool,
) -> Vec<String> {
    let width = usize::from(width);
    let bar_width = width.saturating_sub(LABEL).max(1);
    let (start, len) = session.view();
    let column = |i: usize| {
        (
            start + len * i as u64 / bar_width as u64,
            start + len * (i as u64 + 1) / bar_width as u64,
        )
    };
    let slider = session.slider_ms();
    let slider_col = (0..bar_width)
        .find(|&i| {
            let (a, b) = column(i);
            a <= slider && slider < b
        })
        .unwrap_or(bar_width - 1);

    let mut lines = Vec::new();
    let mut at = format!("  at {}", format_time(slider));
    if let Some((mark, _)) = session.open_mark() {
        at.push_str(&format!(
            "   clip from {}, press c at its end",
            format_time(mark)
        ));
    }
    lines.push(fit(&at, width));

    for &bar in session.bars() {
        let name = match bar {
            Bar::Video => "video",
            Bar::Audio => "audio",
        };
        let chosen = if session.selected() == Some(bar) {
            '>'
        } else {
            ' '
        };
        let label = format!("{name:<6} ");
        let label = if session.highlighted() == Some(bar) && session.bars().len() > 1 {
            paint(&label, "7", styled)
        } else {
            label
        };
        let mut line = format!("{chosen}{label}");
        for i in 0..bar_width {
            let (a, b) = column(i);
            let marks = session
                .clips()
                .iter()
                .map(|c| (c.start_ms, c.end_ms, c.keeps))
                .chain(
                    session
                        .open_mark()
                        .map(|(m, keeps)| (m.min(slider), m.max(slider), keeps)),
                )
                .filter(|&(s, e, keeps)| keeps_bar(keeps, bar) && s < b && e > a);
            let mut inside = false;
            let mut edge = None;
            for (s, e, _) in marks {
                inside = true;
                if (a..b).contains(&s) {
                    edge = Some('[');
                } else if e > a && e <= b && edge.is_none() {
                    edge = Some(']');
                }
            }
            let (ch, colour) = if i == slider_col {
                ('│', Some("1;33".to_string()))
            } else if let Some(e) = edge {
                (e, Some("1".to_string()))
            } else {
                match bar {
                    Bar::Video => match strip.get(i).copied().flatten() {
                        Some(c) => ('█', Some(fg_params(c, truecolor))),
                        None => (' ', None),
                    },
                    Bar::Audio => match levels.get(i).copied().flatten() {
                        Some(db) if db < QUIET_DBFS => ('·', Some("2".to_string())),
                        Some(db) => {
                            let step = ((db + 60.0) / 60.0 * 8.0).clamp(0.0, 7.0) as usize;
                            (LEVELS[step], Some("32".to_string()))
                        }
                        None => (' ', None),
                    },
                }
            };
            let cell = ch.to_string();
            line.push_str(&match (styled, colour, inside) {
                (true, Some(sgr), true) => format!("\x1b[{sgr};44m{cell}\x1b[0m"),
                (true, None, true) => format!("\x1b[44m{cell}\x1b[0m"),
                (true, Some(sgr), false) => format!("\x1b[{sgr}m{cell}\x1b[0m"),
                _ => cell,
            });
        }
        lines.push(line);
    }

    let mut axis = vec![' '; bar_width];
    let mut place = |pos: usize, text: &str| {
        for (k, ch) in text.chars().enumerate() {
            if let Some(slot) = axis.get_mut(pos + k) {
                *slot = ch;
            }
        }
    };
    let left = format_time(start);
    let mid = format_time(start + len / 2);
    let right = format_time(start + len);
    place(0, &left);
    if bar_width > left.len() + mid.len() + right.len() + 4 {
        place(bar_width / 2 - mid.len() / 2, &mid);
    }
    place(bar_width.saturating_sub(right.len()), &right);
    lines.push(format!(
        "{}{}",
        " ".repeat(LABEL),
        axis.into_iter().collect::<String>()
    ));

    let mut clips: Vec<String> = session
        .clips()
        .iter()
        .enumerate()
        .map(|(n, c)| format!("{}) {}", n + 1, clip_words(c.start_ms, c.end_ms, c.keeps)))
        .collect();
    if let Some((m, _)) = session.open_mark() {
        clips.push(format!("{}) {}-", clips.len() + 1, format_time(m)));
    }
    let clips = if clips.is_empty() {
        "  clips  none yet".to_string()
    } else {
        format!("  clips  {}", clips.join("  "))
    };
    lines.push(fit(&clips, width));
    lines.push(match session.message() {
        Some(m) => paint(&fit(&format!("  {m}"), width), "1;33", styled),
        None => String::new(),
    });
    lines.push(fit(
        "  ←/→ move (hold to scrub)  PgUp/PgDn jump  ,/. frame  +/- zoom",
        width,
    ));
    lines.push(fit(
        "  ↑/↓ Enter bar  c start/end  u undo  Esc drop  w write  q quit",
        width,
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trim::session::Input;
    use std::time::Instant;

    #[test]
    fn half_blocks_draw_two_pixels_a_cell() {
        // red green
        // blue white
        let px = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let s = blocks(&px, 2, 2, true, 3, 5);
        assert!(s.starts_with("\x1b[3;5H"), "{s:?}");
        assert!(s.contains("\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m▀"), "{s:?}");
        assert!(
            s.contains("\x1b[38;2;0;255;0m\x1b[48;2;255;255;255m▀"),
            "{s:?}"
        );
        assert!(s.ends_with("\x1b[0m"), "{s:?}");
    }

    #[test]
    fn an_odd_last_row_has_no_bottom_pixel() {
        let px = [10, 20, 30, 10, 20, 30, 40, 50, 60];
        let s = blocks(&px, 1, 3, true, 1, 1);
        assert!(s.contains("\x1b[2;1H"), "two lines: {s:?}");
        assert!(s.contains("\x1b[38;2;40;50;60m\x1b[49m▀"), "{s:?}");
    }

    #[test]
    fn the_256_colour_palette_is_used_without_truecolor() {
        let s = blocks(&[255, 0, 0, 255, 255, 255], 1, 2, false, 1, 1);
        assert!(s.contains("\x1b[38;5;196m\x1b[48;5;231m▀"), "{s:?}");
    }

    #[test]
    fn each_sextant_shape_has_its_character() {
        assert_eq!(sextant(0), ' ');
        assert_eq!(sextant(63), '█');
        assert_eq!(sextant(21), '▌', "the left column");
        assert_eq!(sextant(42), '▐', "the right column");
        // The rest from U+1FB00, named for the parts they fill: 1 top left,
        // 2 top right, 3 and 4 the middle, 5 and 6 the bottom.
        assert_eq!(sextant(1), '\u{1FB00}', "SEXTANT-1");
        assert_eq!(sextant(20), '\u{1FB13}', "SEXTANT-35");
        assert_eq!(sextant(22), '\u{1FB14}', "SEXTANT-235, after ▌");
        assert_eq!(sextant(43), '\u{1FB28}', "SEXTANT-1246, after ▐");
        assert_eq!(sextant(62), '\u{1FB3B}', "SEXTANT-23456");
    }

    #[test]
    fn six_pixels_are_split_into_the_two_colours_that_fit_them_best() {
        const W: [u8; 3] = [255, 255, 255];
        const K: [u8; 3] = [0, 0, 0];
        const R: [u8; 3] = [200, 0, 0];
        const B: [u8; 3] = [0, 0, 200];
        assert_eq!(fit_cell([W, W, K, K, K, K]), (3, W, K), "white over black");
        assert_eq!(fit_cell([R, B, R, B, R, B]), (21, R, B), "red beside blue");
        assert_eq!(fit_cell([R; 6]), (63, R, R), "one colour fills the cell");
        // More than two colours: each part gets the mean of its pixels.
        let reds = [[90, 0, 0], K, [210, 0, 0], K, [150, 0, 0], K];
        assert_eq!(fit_cell(reds), (21, [150, 0, 0], K));
    }

    #[test]
    fn sextants_draw_six_pixels_a_cell() {
        // Two cells: white over black, then all red.
        let (w, k, r) = ([255, 255, 255], [0, 0, 0], [200, 0, 0]);
        let rows = [[w, w, r, r], [k, k, r, r], [k, k, r, r]];
        let px: Vec<u8> = rows.iter().flatten().flatten().copied().collect();
        let s = sextants(&px, 4, 3, true, 3, 5);
        assert!(s.starts_with("\x1b[3;5H"), "{s:?}");
        assert!(
            s.contains("\x1b[38;2;255;255;255m\x1b[48;2;0;0;0m\u{1FB02}"),
            "{s:?}"
        );
        assert!(s.contains("\x1b[38;2;200;0;0m\x1b[48;2;200;0;0m█"), "{s:?}");
        assert!(s.ends_with("\x1b[0m"), "{s:?}");
        let palette = sextants(&px, 4, 3, false, 1, 1);
        assert!(
            palette.contains("\x1b[38;5;231m\x1b[48;5;16m\u{1FB02}"),
            "{palette:?}"
        );
    }

    /// A picture grabbed for an older box can be any size; the last row and
    /// column are repeated where a cell runs past them.
    #[test]
    fn a_picture_that_does_not_fill_its_last_cells_still_draws() {
        let s = sextants(&[1, 2, 3], 1, 1, true, 1, 1);
        assert!(s.contains("\x1b[38;2;1;2;3m\x1b[48;2;1;2;3m█"), "{s:?}");
    }

    #[test]
    fn ansi256_picks_the_nearest_cube_or_grey() {
        assert_eq!(ansi256(0, 0, 0), 16);
        assert_eq!(ansi256(255, 255, 255), 231);
        assert_eq!(ansi256(255, 0, 0), 196);
        assert_eq!(ansi256(128, 128, 128), 244);
        assert_eq!(ansi256(0, 135, 255), 33);
    }

    #[test]
    fn kitty_clears_then_sends_rgb_in_chunks_asking_for_no_reply() {
        let s = kitty(&[1, 2, 3, 4, 5, 6], 2, 1, 10, 2, 7);
        assert!(s.starts_with("\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2;7H"), "{s:?}");
        assert!(
            s.contains("\x1b_Ga=T,f=24,s=2,v=1,c=10,q=2,m=0;AQIDBAUG\x1b\\"),
            "{s:?}"
        );
        let big = vec![7u8; 4_000]; // 5336 base64 characters: two chunks
        let s = kitty(&big, 40, 33, 10, 1, 1);
        assert_eq!(s.matches("\x1b_G").count(), 3, "delete, then two chunks");
        assert!(s.contains(",m=1;"), "the first chunk says more follows");
        assert!(s.contains("\x1b_Gm=0;"), "the last says it is the last");
        for part in s.split("\x1b_G").skip(2) {
            let payload = part.split(';').nth(1).unwrap().trim_end_matches("\x1b\\");
            assert!(payload.len() <= 4096, "{}", payload.len());
        }
    }

    #[test]
    fn iterm_sends_the_png_inline_at_a_width_in_cells() {
        let s = iterm(&[1, 2, 3, 4], 30, 2, 9);
        assert_eq!(
            s,
            "\x1b[2;9H\x1b]1337;File=inline=1;size=4;width=30;preserveAspectRatio=1:AQIDBA==\x07"
        );
    }

    #[test]
    fn the_picture_fills_what_the_text_leaves_keeping_its_shape() {
        let l = layout(80, 24, Some((16, 9)), 2).unwrap();
        assert_eq!(
            l.image,
            Some(ImageBox {
                row: 2,
                col: 14,
                cols: 53,
                rows: 15
            })
        );
        assert_eq!(l.below, 17);
        let tall = layout(80, 24, Some((9, 16)), 2).unwrap().image.unwrap();
        assert_eq!((tall.cols, tall.rows), (16, 15));
        let audio = layout(80, 24, None, 1).unwrap();
        assert_eq!((audio.image, audio.below), (None, 2));
        assert_eq!(layout(30, 10, Some((16, 9)), 2), None, "too small");
    }

    /// The size it asks for is the size `layout` draws in, and a question
    /// still shows, so q isn't answered blind.
    #[test]
    fn a_terminal_too_small_says_the_size_it_needs_and_any_question() {
        for (aspect, bars, rows) in [(Some((16, 9)), 2, 12), (None, 1, 8)] {
            assert!(layout(40, rows, aspect, bars).is_some(), "{rows}");
            assert_eq!(layout(40, rows - 1, aspect, bars), None, "{rows}");
            assert_eq!(layout(39, rows, aspect, bars), None, "{rows}");
            let text = too_small(aspect, bars, None);
            assert!(
                text.contains(&format!("40 columns wide and {rows} rows high")),
                "{text}"
            );
        }
        let asking = too_small(None, 1, Some("Quit without writing 1 clip? [y/N]"));
        assert!(
            asking.ends_with("\r\nQuit without writing 1 clip? [y/N]"),
            "{asking}"
        );
    }

    #[test]
    fn loudness_columns_average_their_buckets_and_wait_for_the_rest() {
        let mut buckets = vec![1.0; 50];
        buckets.extend(vec![0.0; 50]);
        let cols = loudness_columns(&buckets, (0, 1_000), 2);
        assert_eq!(cols, [Some(0.0), Some(-90.0)]);
        let cols = loudness_columns(&buckets[..50], (0, 1_000), 2);
        assert_eq!(cols, [Some(0.0), None], "the second half is not read yet");
    }

    #[test]
    fn column_times_split_the_view_evenly() {
        assert_eq!(
            column_times((1_000, 4_000), 4),
            [1_000, 2_000, 3_000, 4_000]
        );
    }

    fn marked() -> Session {
        // 60 s; a clip of 6-12 s kept as audio; the slider at 30 s.
        let mut s = Session::new(60_000, Some((30, 1)), true, true);
        let t = Instant::now();
        for i in [Input::Down, Input::Enter] {
            s.input(i, t);
        }
        for i in [Input::PageDown, Input::Cut, Input::PageDown, Input::Cut] {
            s.input(i, t);
        }
        for _ in 0..3 {
            s.input(Input::PageDown, t);
        }
        s
    }

    #[test]
    fn the_bars_show_the_slider_and_each_clip_on_what_it_keeps() {
        let s = marked();
        let width = 70u16;
        let lines = below(&s, &[None; 60], &[Some(-20.0); 60], width, false, false);
        let video = lines.iter().find(|l| l.contains("video")).unwrap();
        let audio = lines.iter().find(|l| l.contains("audio")).unwrap();
        assert!(audio.starts_with('>'), "the selected bar: {audio:?}");
        assert!(audio.contains('[') && audio.contains(']'), "{audio:?}");
        assert!(!video.contains('['), "the clip keeps no picture: {video:?}");
        // The slider sits at the same column on both bars.
        let col = |l: &str| l.chars().position(|c| c == '│');
        assert_eq!(col(video), col(audio));
        assert!(col(video).is_some());
        for l in &lines {
            assert!(l.chars().count() <= usize::from(width), "{l:?}");
        }
        assert!(
            lines.iter().any(|l| l.contains("0:06-0:12 audio")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("0:30")),
            "the slider's time"
        );
        assert!(lines.iter().any(|l| l.contains("w write")), "the keys");
    }

    #[test]
    fn quiet_columns_are_drawn_as_dots() {
        let s = Session::new(10_000, None, false, true);
        let lines = below(&s, &[], &[Some(-10.0), Some(-70.0), None], 13, false, false);
        let audio = lines.iter().find(|l| l.contains("audio")).unwrap();
        assert!(audio.contains('·'), "{audio:?}");
    }

    #[test]
    fn the_header_names_the_file() {
        let s = Session::new(761_000, Some((30, 1)), true, true);
        let info = Info {
            name: "talk.mp4".into(),
            size: Some((1920, 1080)),
            fps: Some("30".into()),
        };
        let h = header(&info, &s, 80, false);
        assert_eq!(h, "talk.mp4  12:41  1920x1080  30 fps");
    }
}
