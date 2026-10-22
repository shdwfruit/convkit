//! The terminal a full-screen command (`conv trim`, `conv ascii -i`)
//! holds for its session and gives back as it was found, with keys read on
//! their own thread.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use console::{Key, Term};

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
/// Ctrl-C arrives as a key, for the command to treat as quitting. Dropping
/// the guard, or a panic, gives all of it back.
pub struct Screen {
    kitty_pictures: bool,
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
    /// Holds the terminal. `kitty_pictures` says pictures were drawn with
    /// kitty's protocol, which keeps them until they are deleted, even
    /// after the alternate screen is left.
    pub fn enter(kitty_pictures: bool) -> Screen {
        hold_tty();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(kitty_pictures);
            previous(info);
        }));
        ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        put(ENTER);
        Screen {
            kitty_pictures,
            stop: Arc::new(AtomicBool::new(false)),
            keys: None,
        }
    }

    /// Reads keys on their own thread for the rest of the session, handing
    /// each to `send` with the moment it arrived, until `send` returns
    /// false or the session ends.
    pub fn read_keys(&mut self, send: impl Fn(Key, Instant) -> bool + Send + 'static) {
        let stop = Arc::clone(&self.stop);
        self.keys = Some(std::thread::spawn(move || {
            let term = Term::stdout();
            while !stop.load(Ordering::SeqCst) {
                if !key_waiting() {
                    continue;
                }
                // `read_key_raw` hands back Ctrl-C as a key instead of
                // letting it stop the process, so the command can quit
                // properly.
                let Ok(key) = term.read_key_raw() else { break };
                if !send(key, Instant::now()) {
                    break;
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
        restore(self.kitty_pictures);
    }
}

fn restore(kitty_pictures: bool) {
    if !ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if kitty_pictures {
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
