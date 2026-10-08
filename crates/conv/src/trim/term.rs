//! The terminal `conv trim` runs in: held for the session and given back
//! as it was found, keys read on their own thread, and the way pictures
//! are drawn chosen from what the terminal says it is.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use console::{Key, Term};

use super::draw::Graphics;
use super::session::Input;

/// How to draw the picture: `--graphics` when given, otherwise what the
/// environment says the terminal is. Only the environment is read, never
/// the terminal itself: asking it means reading a reply with a timeout,
/// and the reply goes astray under tmux.
pub fn choose(choice: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Graphics {
    let is = |var: &str, value: &str| env(var).is_some_and(|v| v == value);
    let truecolor =
        is("COLORTERM", "truecolor") || is("COLORTERM", "24bit") || env("WT_SESSION").is_some();
    let blocks = Graphics::Blocks { truecolor };
    match choice {
        Some("blocks") => return blocks,
        Some("kitty") => return Graphics::Kitty,
        Some("iterm") => return Graphics::Iterm,
        _ => {}
    }
    // tmux passes neither protocol through without its own wrapping.
    if env("TMUX").is_some() {
        return blocks;
    }
    if is("TERM", "xterm-kitty") || is("TERM", "xterm-ghostty") || is("TERM_PROGRAM", "ghostty") {
        return Graphics::Kitty;
    }
    if is("TERM_PROGRAM", "iTerm.app") || is("TERM_PROGRAM", "WezTerm") {
        return Graphics::Iterm;
    }
    blocks
}

/// A key as the session understands it.
pub fn input_for(key: &Key) -> Input {
    match key {
        Key::ArrowLeft => Input::Left,
        Key::ArrowRight => Input::Right,
        Key::ArrowUp => Input::Up,
        Key::ArrowDown => Input::Down,
        Key::PageUp => Input::PageUp,
        Key::PageDown => Input::PageDown,
        Key::Home => Input::Home,
        Key::End => Input::End,
        Key::Enter => Input::Enter,
        Key::Escape => Input::Escape,
        Key::CtrlC | Key::Char('\u{3}') => Input::Quit,
        Key::Char(c) => match c.to_ascii_lowercase() {
            'c' => Input::Cut,
            'u' => Input::Undo,
            'w' => Input::Write,
            'q' => Input::Quit,
            'y' => Input::Yes,
            '+' | '=' => Input::ZoomIn,
            '-' | '_' => Input::ZoomOut,
            ',' | '<' => Input::FrameBack,
            '.' | '>' => Input::FrameForward,
            _ => Input::Other,
        },
        _ => Input::Other,
    }
}

/// Whether a key is waiting to be read. On unix the key thread waits here,
/// a tenth of a second at a time, rather than inside `console`'s read: that
/// read holds the tty in raw mode and puts back the mode it found when a
/// key arrives, so a thread left waiting in it after the session would
/// undo the session's restore on the next keypress.
#[cfg(unix)]
fn key_waiting() -> bool {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, valid for the call.
    unsafe { libc::poll(&mut fd, 1, 100) > 0 }
}

#[cfg(not(unix))]
fn key_waiting() -> bool {
    true
}

/// The terminal's size, columns then rows.
pub fn size() -> (u16, u16) {
    let (rows, cols) = Term::stdout().size();
    (cols, rows)
}

/// Writes to the terminal and flushes, so a frame appears whole.
pub fn put(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// The alternate screen (so the user's scrollback is left as it was), the
/// cursor hidden, and on unix the tty's mode held for the whole session:
/// no echo, no line editing, no signals, no flow control. `console`
/// switches the tty to raw mode around each key it reads and restores it
/// between them, so without this a held arrow had its escape sequence
/// echoed onto the screen between two reads, and a Ctrl-C between two
/// reads stopped conv with the screen left as it was. With signals off,
/// Ctrl-C arrives as a key and goes to the quit question. Dropping the
/// guard, or a panic, gives all of it back.
pub struct Screen {
    graphics: Graphics,
    stop: Arc<AtomicBool>,
    keys: Option<std::thread::JoinHandle<()>>,
}

/// The tty mode before the session, for `Drop` and the panic hook.
#[cfg(unix)]
static SAVED: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

/// Whether a session holds the terminal, so the panic hook, which outlives
/// the session, only gives back what is still held.
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J";
const LEAVE: &str = "\x1b[0m\x1b[?25h\x1b[?1049l";

impl Screen {
    pub fn enter(graphics: Graphics) -> Screen {
        hold_tty();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(None);
            previous(info);
        }));
        ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        put(ENTER);
        Screen {
            graphics,
            stop: Arc::new(AtomicBool::new(false)),
            keys: None,
        }
    }

    /// Reads keys on their own thread for the rest of the session, handing
    /// each to `send` with the moment it arrived, until `send` returns
    /// false or the session ends.
    pub fn read_keys(&mut self, send: impl Fn(Input, Instant) -> bool + Send + 'static) {
        let stop = Arc::clone(&self.stop);
        self.keys = Some(std::thread::spawn(move || {
            let term = Term::stdout();
            while !stop.load(Ordering::SeqCst) {
                if !key_waiting() {
                    continue;
                }
                // `read_key_raw` hands back Ctrl-C as a key instead of
                // letting it stop the process, so it reaches the quit
                // question.
                match term.read_key_raw() {
                    Ok(key) if send(input_for(&key), Instant::now()) => {}
                    _ => break,
                }
            }
        }));
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // On unix the thread sees the flag within one wait. Elsewhere it may
        // be inside a read, which holds nothing to undo, and ends with the
        // process.
        if cfg!(unix) {
            if let Some(keys) = self.keys.take() {
                let _ = keys.join();
            }
        }
        restore(Some(self.graphics));
    }
}

fn restore(graphics: Option<Graphics>) {
    if !ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if graphics == Some(Graphics::Kitty) {
        // kitty keeps a picture until it is deleted, even after the
        // alternate screen is left.
        put("\x1b_Ga=d,d=A,q=2\x1b\\");
    }
    put(LEAVE);
    release_tty();
}

#[cfg(unix)]
fn hold_tty() {
    // SAFETY: tcgetattr/tcsetattr on stdin with a zeroed termios they
    // fill in; a failure (stdin is not a tty) leaves everything as it is.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
            return;
        }
        let saved = t;
        t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        t.c_iflag &= !libc::IXON;
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &t) == 0 {
            if let Ok(mut s) = SAVED.lock() {
                *s = Some(saved);
            }
        }
    }
}

#[cfg(unix)]
fn release_tty() {
    let saved = SAVED.lock().ok().and_then(|mut s| s.take());
    if let Some(t) = saved {
        // SAFETY: restores the termios read from this same fd.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &t);
        }
    }
}

// On Windows, `console` turns off Ctrl-C processing while it waits for a
// key, and the key thread is always waiting for one.
#[cfg(not(unix))]
fn hold_tty() {}

#[cfg(not(unix))]
fn release_tty() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_terminal_is_told_from_its_environment() {
        let cases: &[(&[(&str, &str)], Graphics)] = &[
            (&[("TERM", "xterm-kitty")], Graphics::Kitty),
            (&[("TERM", "xterm-ghostty")], Graphics::Kitty),
            (&[("TERM_PROGRAM", "ghostty")], Graphics::Kitty),
            (&[("TERM_PROGRAM", "iTerm.app")], Graphics::Iterm),
            (&[("TERM_PROGRAM", "WezTerm")], Graphics::Iterm),
            (
                &[("COLORTERM", "truecolor")],
                Graphics::Blocks { truecolor: true },
            ),
            (
                &[("COLORTERM", "24bit")],
                Graphics::Blocks { truecolor: true },
            ),
            (&[("WT_SESSION", "x")], Graphics::Blocks { truecolor: true }),
            (&[], Graphics::Blocks { truecolor: false }),
            // tmux would need its own escapes passed through: half blocks.
            (
                &[
                    ("TMUX", "/tmp/s"),
                    ("TERM", "xterm-kitty"),
                    ("COLORTERM", "truecolor"),
                ],
                Graphics::Blocks { truecolor: true },
            ),
        ];
        for (pairs, want) in cases {
            assert_eq!(choose(None, &env(pairs)), *want, "{pairs:?}");
        }
    }

    #[test]
    fn graphics_overrides_what_the_environment_says() {
        let kitty = env(&[("TERM", "xterm-kitty"), ("COLORTERM", "truecolor")]);
        assert_eq!(
            choose(Some("blocks"), &kitty),
            Graphics::Blocks { truecolor: true }
        );
        assert_eq!(choose(Some("iterm"), &kitty), Graphics::Iterm);
        assert_eq!(choose(Some("kitty"), &env(&[])), Graphics::Kitty);
    }

    #[test]
    fn keys_map_to_what_they_do() {
        let cases = [
            (Key::ArrowLeft, Input::Left),
            (Key::ArrowRight, Input::Right),
            (Key::ArrowUp, Input::Up),
            (Key::ArrowDown, Input::Down),
            (Key::PageUp, Input::PageUp),
            (Key::PageDown, Input::PageDown),
            (Key::Home, Input::Home),
            (Key::End, Input::End),
            (Key::Enter, Input::Enter),
            (Key::Escape, Input::Escape),
            (Key::CtrlC, Input::Quit),
            (Key::Char('\u{3}'), Input::Quit),
            (Key::Char('c'), Input::Cut),
            (Key::Char('u'), Input::Undo),
            (Key::Char('w'), Input::Write),
            (Key::Char('q'), Input::Quit),
            (Key::Char('y'), Input::Yes),
            (Key::Char('Y'), Input::Yes),
            (Key::Char('+'), Input::ZoomIn),
            (Key::Char('='), Input::ZoomIn),
            (Key::Char('-'), Input::ZoomOut),
            (Key::Char(','), Input::FrameBack),
            (Key::Char('.'), Input::FrameForward),
            (Key::Char('x'), Input::Other),
            (Key::Tab, Input::Other),
        ];
        for (key, want) in cases {
            assert_eq!(input_for(&key), want, "{key:?}");
        }
    }
}
