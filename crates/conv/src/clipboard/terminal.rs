//! The terminal's own clipboard, through kitty's clipboard protocol
//! (OSC 5522): any type of data, not only text, and it reaches the
//! clipboard of the computer the terminal runs on, so it also works over
//! SSH. kitty and Ghostty (1.4 and later) speak it.
//!
//! The escapes are built and read by pure functions, tested below; `Tty`
//! does the talking, on unix only.

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;

/// Reads base64 with or without its padding, as terminals may send either.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The most raw bytes in one packet, which the protocol sets.
const CHUNK: usize = 4096;

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn osc(metadata: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!("\x1b]5522;{metadata}\x1b\\")
    } else {
        format!("\x1b]5522;{metadata};{payload}\x1b\\")
    }
}

/// Asks whether the terminal speaks the protocol (DECRQM on mode 5522),
/// then for its primary attributes (DA1). Every terminal answers DA1, so
/// its answer arriving first means no answer to the first is coming, with
/// no need to wait out a timeout.
pub const SUPPORT_QUERY: &str = "\x1b[?5522$p\x1b[c";

/// What the answers to `SUPPORT_QUERY` say so far: `Some` once they are
/// complete enough to tell.
pub fn supported_from(answer: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(answer);
    if let Some(at) = text.find("\x1b[?5522;") {
        let rest = &text[at + "\x1b[?5522;".len()..];
        if let Some(end) = rest.find("$y") {
            // 0 is "not recognised" and 4 "permanently reset"; 1, 2 and 3
            // are set or reset, which a terminal can only say of a mode
            // it has.
            return Some(!matches!(rest[..end].trim(), "0" | "4"));
        }
    }
    // A DA1 answer is `CSI ? ... c`, and it comes after the DECRQM one.
    let da1 = text.match_indices("\x1b[?").any(|(at, _)| {
        let rest = &text[at + 3..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(rest.len());
        rest[end..].starts_with('c')
    });
    da1.then_some(false)
}

/// The escapes that put `items`, each a MIME type and its bytes, on the
/// clipboard together.
pub fn write_escapes(items: &[(&str, &[u8])]) -> String {
    let mut s = osc("type=write", "");
    for (mime, bytes) in items {
        let mime = b64(mime.as_bytes());
        for chunk in bytes.chunks(CHUNK) {
            s.push_str(&osc(&format!("type=wdata:mime={mime}"), &b64(chunk)));
        }
    }
    s.push_str(&osc("type=wdata", ""));
    s
}

/// The escape that asks for the clipboard's data in `mimes`; `["."]` asks
/// which types it holds.
pub fn read_escape(mimes: &[&str]) -> String {
    osc("type=read", &b64(mimes.join(" ").as_bytes()))
}

/// One OSC 5522 packet from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: String,
    pub status: Option<String>,
    pub mime: Option<String>,
    pub payload: Vec<u8>,
}

/// Takes every whole OSC 5522 packet off the front of `buf`, leaving what
/// is not yet complete, and drops anything else the terminal sent.
pub fn take_packets(buf: &mut Vec<u8>) -> Vec<Packet> {
    const START: &[u8] = b"\x1b]5522;";
    let mut packets = Vec::new();
    loop {
        let Some(at) = find(buf, START) else {
            // Keep a tail that could be the start of a packet.
            let keep = buf.len().min(START.len() - 1);
            buf.drain(..buf.len() - keep);
            return packets;
        };
        let body = at + START.len();
        let end = (body..buf.len()).find_map(|i| match buf[i] {
            0x07 => Some((i, 1)),
            0x1b if buf.get(i + 1) == Some(&b'\\') => Some((i, 2)),
            _ => None,
        });
        let Some((end, len)) = end else {
            buf.drain(..at);
            return packets;
        };
        if let Some(p) = parse(&buf[body..end]) {
            packets.push(p);
        }
        buf.drain(..end + len);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse(body: &[u8]) -> Option<Packet> {
    let body = std::str::from_utf8(body).ok()?;
    let (meta, payload) = body.split_once(';').unwrap_or((body, ""));
    let mut p = Packet {
        kind: String::new(),
        status: None,
        mime: None,
        payload: LENIENT.decode(payload).ok()?,
    };
    for pair in meta.split(':') {
        match pair.split_once('=') {
            Some(("type", v)) => p.kind = v.to_string(),
            Some(("status", v)) => p.status = Some(v.to_string()),
            Some(("mime", v)) => p.mime = Some(String::from_utf8(LENIENT.decode(v).ok()?).ok()?),
            _ => {}
        }
    }
    Some(p)
}

/// How a read ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// Still waiting for more.
    Pending,
    /// Each type asked for, with its bytes.
    Done(Vec<(String, Vec<u8>)>),
    /// The terminal's error code: EPERM when the user said no.
    Refused(String),
}

/// Gathers a read's answer from `packets`, added to `got` as they come.
pub fn read_progress(got: &mut Vec<(String, Vec<u8>)>, packets: &[Packet]) -> Read {
    for p in packets.iter().filter(|p| p.kind == "read") {
        match p.status.as_deref() {
            Some("DATA") => {
                let mime = p.mime.clone().unwrap_or_default();
                match got.iter_mut().find(|(m, _)| *m == mime) {
                    Some((_, bytes)) => bytes.extend_from_slice(&p.payload),
                    None => got.push((mime, p.payload.clone())),
                }
            }
            Some("DONE") => return Read::Done(std::mem::take(got)),
            Some("OK") | None => {}
            Some(code) => return Read::Refused(code.to_string()),
        }
    }
    Read::Pending
}

/// The types the terminal said the clipboard holds, from the answer to
/// `read_escape(&["."])`: one DATA packet a type, or one whose data is the
/// list itself.
pub fn types_listed(answer: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut types = Vec::new();
    for (mime, data) in answer {
        if mime == "." || (mime.is_empty() && !data.is_empty()) {
            types.extend(
                String::from_utf8_lossy(data)
                    .split_whitespace()
                    .map(str::to_string),
            );
        } else if !mime.is_empty() {
            types.push(mime.clone());
        }
    }
    types.dedup();
    types
}

/// What a write's answer says: `Some(Ok)` once done, `Some(Err)` with the
/// error code.
pub fn write_result(packets: &[Packet]) -> Option<Result<(), String>> {
    packets
        .iter()
        .filter(|p| p.kind == "write")
        .find_map(|p| match p.status.as_deref() {
            Some("DONE") => Some(Ok(())),
            Some("OK") | None => None,
            Some(code) => Some(Err(code.to_string())),
        })
}

/// Says what a terminal error code means.
pub fn explain(code: &str) -> String {
    match code {
        "EPERM" => "the terminal wasn't allowed to".to_string(),
        "EFBIG" => "it is more than the terminal takes".to_string(),
        "EBUSY" => "the terminal was busy; try again".to_string(),
        other => format!("the terminal said {other}"),
    }
}

#[cfg(unix)]
pub use tty::{Answer, Tty};

#[cfg(unix)]
mod tty {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    /// The controlling terminal, in raw mode until dropped, so its answers
    /// arrive unechoed and as they come.
    pub struct Tty {
        file: std::fs::File,
        saved: libc::termios,
    }

    impl Tty {
        pub fn open() -> Option<Tty> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok()?;
            let fd = file.as_raw_fd();
            // SAFETY: tcgetattr/tcsetattr on an open tty fd, with a termios
            // they fill in.
            let saved = unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut t) != 0 {
                    return None;
                }
                let saved = t;
                // Signals off too: a Ctrl-C while waiting on the terminal
                // arrives as a byte, so the mode is put back before conv
                // stops, rather than left without echo.
                t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
                t.c_cc[libc::VMIN] = 0;
                t.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                    return None;
                }
                saved
            };
            Some(Tty { file, saved })
        }

        pub fn send(&mut self, s: &str) -> bool {
            self.file.write_all(s.as_bytes()).is_ok() && self.file.flush().is_ok()
        }

        /// Reads what the terminal sends until `done` says it has enough,
        /// `wait` passes, or the user presses Ctrl-C, whichever is first.
        pub fn answer<T>(
            &mut self,
            wait: Duration,
            mut done: impl FnMut(&mut Vec<u8>) -> Option<T>,
        ) -> Answer<T> {
            let until = Instant::now() + wait;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 65536];
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Answer::TimedOut;
                }
                // select, not poll: macOS's poll doesn't take a tty.
                let fd = self.file.as_raw_fd();
                let mut timeout = libc::timeval {
                    tv_sec: left.as_secs() as libc::time_t,
                    tv_usec: left.subsec_micros() as libc::suseconds_t,
                };
                // SAFETY: an fd_set zeroed then filled with one open fd,
                // and a timeval, both valid for the call.
                let ready = unsafe {
                    let mut set: libc::fd_set = std::mem::zeroed();
                    libc::FD_ZERO(&mut set);
                    libc::FD_SET(fd, &mut set);
                    libc::select(
                        fd + 1,
                        &mut set,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut timeout,
                    )
                };
                if ready <= 0 {
                    continue;
                }
                // SAFETY: reads into a buffer of the length given.
                let n = unsafe {
                    libc::read(
                        self.file.as_raw_fd(),
                        chunk.as_mut_ptr().cast(),
                        chunk.len(),
                    )
                };
                // The terminal hung up.
                if n <= 0 {
                    return Answer::TimedOut;
                }
                let got = &chunk[..n as usize];
                // Ctrl-C, not inside an answer (base64 never holds it).
                if got.contains(&0x03) {
                    return Answer::Stopped;
                }
                buf.extend_from_slice(got);
                if let Some(t) = done(&mut buf) {
                    return Answer::Got(t);
                }
            }
        }
    }

    /// How waiting on the terminal ended.
    pub enum Answer<T> {
        Got(T),
        TimedOut,
        /// The user pressed Ctrl-C.
        Stopped,
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            // SAFETY: restores the termios read from this same fd. An
            // answer half read is dropped, not left for the shell.
            unsafe {
                libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH);
                libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_is_told_from_decrqm_or_from_da1_coming_first() {
        assert_eq!(supported_from(b""), None);
        assert_eq!(supported_from(b"\x1b[?5522;2$y"), Some(true));
        assert_eq!(supported_from(b"\x1b[?5522;0$y\x1b[?62;22c"), Some(false));
        assert_eq!(supported_from(b"\x1b[?5522;4$y"), Some(false));
        // No DECRQM answer at all, only DA1.
        assert_eq!(supported_from(b"\x1b[?1;2c"), Some(false));
        assert_eq!(supported_from(b"\x1b[?62;22"), None);
    }

    #[test]
    fn a_write_starts_sends_chunks_and_ends() {
        let data = vec![7u8; CHUNK + 10];
        let s = write_escapes(&[("image/png", &data)]);
        let mime = b64(b"image/png");
        assert!(s.starts_with("\x1b]5522;type=write\x1b\\"), "{s:?}");
        assert_eq!(s.matches(&format!("type=wdata:mime={mime};")).count(), 2);
        assert!(s.ends_with("\x1b]5522;type=wdata\x1b\\"));
        // Every chunk is base64 of at most CHUNK bytes.
        let mut buf = s.into_bytes();
        let packets = take_packets(&mut buf);
        let sent: usize = packets.iter().map(|p| p.payload.len()).sum();
        assert_eq!(sent, CHUNK + 10);
        assert!(packets.iter().all(|p| p.payload.len() <= CHUNK));
    }

    #[test]
    fn a_read_asks_for_the_types_in_base64() {
        assert_eq!(
            read_escape(&["text/plain", "image/png"]),
            format!("\x1b]5522;type=read;{}\x1b\\", b64(b"text/plain image/png"))
        );
    }

    #[test]
    fn packets_are_taken_whole_and_a_partial_one_waits() {
        let png = b64(b"image/png");
        let mut buf = format!(
            "junk\x1b]5522;type=read:status=OK\x1b\\\x1b]5522;type=read:status=DATA:mime={png};{}\x07\x1b]5522;type=read:sta",
            b64(b"abc")
        )
        .into_bytes();
        let packets = take_packets(&mut buf);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].mime.as_deref(), Some("image/png"));
        assert_eq!(packets[1].payload, b"abc");
        assert!(buf.starts_with(b"\x1b]5522;type=read:sta"));
    }

    #[test]
    fn unpadded_base64_is_read_too() {
        let mut buf = b"\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;YWI\x1b\\".to_vec();
        let packets = take_packets(&mut buf);
        assert_eq!(packets[0].mime.as_deref(), Some("image/png"));
        assert_eq!(packets[0].payload, b"ab");
    }

    #[test]
    fn a_read_joins_chunks_until_done_or_refused() {
        let png = b64(b"image/png");
        let mut buf = format!(
            "\x1b]5522;type=read:status=OK\x1b\\\x1b]5522;type=read:status=DATA:mime={png};{}\x1b\\\x1b]5522;type=read:status=DATA:mime={png};{}\x1b\\",
            b64(b"ab"),
            b64(b"cd")
        )
        .into_bytes();
        let mut got = Vec::new();
        assert_eq!(
            read_progress(&mut got, &take_packets(&mut buf)),
            Read::Pending
        );
        let mut buf = b"\x1b]5522;type=read:status=DONE\x1b\\".to_vec();
        assert_eq!(
            read_progress(&mut got, &take_packets(&mut buf)),
            Read::Done(vec![("image/png".to_string(), b"abcd".to_vec())])
        );
        let mut buf = b"\x1b]5522;type=read:status=EPERM\x1b\\".to_vec();
        assert_eq!(
            read_progress(&mut Vec::new(), &take_packets(&mut buf)),
            Read::Refused("EPERM".to_string())
        );
    }

    #[test]
    fn the_type_list_reads_either_way_it_comes() {
        let one_each = vec![
            ("image/png".to_string(), vec![]),
            ("text/plain".to_string(), vec![]),
        ];
        assert_eq!(types_listed(&one_each), ["image/png", "text/plain"]);
        let as_data = vec![(".".to_string(), b"image/png text/plain".to_vec())];
        assert_eq!(types_listed(&as_data), ["image/png", "text/plain"]);
    }

    #[test]
    fn a_write_is_done_or_refused() {
        let mut buf = b"\x1b]5522;type=write:status=DONE\x1b\\".to_vec();
        assert_eq!(write_result(&take_packets(&mut buf)), Some(Ok(())));
        let mut buf = b"\x1b]5522;type=write:status=EPERM\x1b\\".to_vec();
        assert_eq!(
            write_result(&take_packets(&mut buf)),
            Some(Err("EPERM".to_string()))
        );
        assert_eq!(write_result(&[]), None);
    }
}
