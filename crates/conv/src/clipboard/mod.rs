//! The clipboard: text for `conv ascii --clipboard`, and files, pictures
//! and text for `--copy` and `--paste`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::Duration;

use base64::Engine;
use convkit_core::{Format, Kind};

#[cfg(target_os = "macos")]
mod macos;
pub mod terminal;
#[cfg(all(unix, not(target_os = "macos")))]
mod unix;

/// How the text was copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// By this program or API, which says whether it worked.
    Tool(&'static str),
    /// By asking the terminal (OSC 52), which can't say whether it did.
    Osc52,
    /// By the terminal, through kitty's clipboard protocol; `confirmed`
    /// when it said it had.
    Terminal { confirmed: bool },
}

/// What `--copy` puts on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Put {
    /// The text of a text file: html is also offered as rich text where
    /// the clipboard has a place for it.
    Text {
        file: PathBuf,
        text: String,
        html: bool,
    },
    /// A picture: the file, where the clipboard can hold one, and its
    /// pixels, so it pastes both as an attachment and into an editor.
    Picture { file: PathBuf, format: Format },
    /// Files, pasted where files go: a folder, a chat, a mail.
    Files(Vec<PathBuf>),
}

/// What `--paste` found on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Got {
    Files(Vec<PathBuf>),
    Picture { format: Format, bytes: Vec<u8> },
    Text(String),
}

/// The pictures a clipboard can carry, most wanted first when pasting,
/// with their MIME types and macOS type identifiers.
pub const PICTURES: &[(Format, &str, &str)] = &[
    (Format::Png, "image/png", "public.png"),
    (Format::Jpg, "image/jpeg", "public.jpeg"),
    (Format::Gif, "image/gif", "com.compuserve.gif"),
    (Format::Webp, "image/webp", "org.webmproject.webp"),
    (Format::Heic, "image/heic", "public.heic"),
    (Format::Avif, "image/avif", "public.avif"),
    (Format::Tiff, "image/tiff", "public.tiff"),
    (Format::Bmp, "image/bmp", "com.microsoft.bmp"),
    (Format::Svg, "image/svg+xml", "public.svg-image"),
    (Format::Ico, "image/x-icon", "com.microsoft.ico"),
];

pub fn mime_of(format: Format) -> Option<&'static str> {
    PICTURES.iter().find(|p| p.0 == format).map(|p| p.1)
}

/// The most wanted picture among the clipboard's `types`, whose names
/// `name` picks from a `PICTURES` row (MIME type or identifier).
pub fn best_picture(
    types: &[String],
    name: fn(&(Format, &'static str, &'static str)) -> &'static str,
) -> Option<(Format, &str)> {
    PICTURES.iter().find_map(|p| {
        types
            .iter()
            .find(|t| t.eq_ignore_ascii_case(name(p)))
            .map(|t| (p.0, t.as_str()))
    })
}

/// What `--copy` puts on the clipboard for what a run wrote.
pub fn put_for(outputs: &[PathBuf]) -> Result<Put, String> {
    let [one] = outputs else {
        return Ok(Put::Files(outputs.to_vec()));
    };
    let format = Format::from_path(one);
    match format {
        Some(Format::Md | Format::Csv | Format::Html) => {
            let text = std::fs::read_to_string(one)
                .map_err(|e| format!("couldn't read {}: {e}", one.display()))?;
            Ok(Put::Text {
                file: one.clone(),
                text,
                html: format == Some(Format::Html),
            })
        }
        Some(f) if f.kind() == Kind::Image && mime_of(f).is_some() => Ok(Put::Picture {
            file: one.clone(),
            format: f,
        }),
        _ => Ok(Put::Files(vec![one.clone()])),
    }
}

/// Whether conv runs over SSH, where the system clipboard is the server's
/// and only the terminal reaches the user's.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// Puts `put` on the clipboard: on this computer the system's own way
/// first, then the terminal's; over SSH the terminal's first.
pub fn put(put: &Put) -> Result<Copied, String> {
    if over_ssh() {
        if let Some(done) = put_through_terminal(put) {
            return done;
        }
    }
    match system_put(put) {
        Some(Ok(tool)) => return Ok(Copied::Tool(tool)),
        Some(Err(e)) => return Err(e),
        None => {}
    }
    if !over_ssh() {
        if let Some(done) = put_through_terminal(put) {
            return done;
        }
    }
    if let Put::Text { text, .. } = put {
        if std::io::stderr().is_terminal() {
            eprint!("{}", osc52(text));
            return Ok(Copied::Osc52);
        }
    }
    Err(no_clipboard())
}

/// Takes what the clipboard holds, the same ways round as `put`. Text
/// that names files is those files.
pub fn get() -> Result<Got, String> {
    get_any().map(|got| match got {
        Got::Text(text) => files_in_text(&text).map_or(Got::Text(text), Got::Files),
        got => got,
    })
}

fn get_any() -> Result<Got, String> {
    if over_ssh() {
        if let Some(got) = get_through_terminal() {
            return got;
        }
    }
    if let Some(got) = system_get() {
        return got;
    }
    if !over_ssh() {
        if let Some(got) = get_through_terminal() {
            return got;
        }
    }
    Err(no_clipboard())
}

fn no_clipboard() -> String {
    if cfg!(any(target_os = "macos", windows)) {
        "this terminal can't reach the clipboard".to_string()
    } else {
        "no clipboard here: install wl-clipboard or xclip, or use a terminal that speaks kitty's clipboard protocol (kitty, Ghostty)".to_string()
    }
}

#[cfg(target_os = "macos")]
fn system_put(put: &Put) -> Option<Result<&'static str, String>> {
    Some(macos::put(put).map(|()| "the macOS clipboard"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_put(put: &Put) -> Option<Result<&'static str, String>> {
    unix::put(put)
}

#[cfg(windows)]
fn system_put(put: &Put) -> Option<Result<&'static str, String>> {
    Some(windows::put(put).map(|()| "the Windows clipboard"))
}

#[cfg(target_os = "macos")]
fn system_get() -> Option<Result<Got, String>> {
    Some(macos::get())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_get() -> Option<Result<Got, String>> {
    unix::get()
}

#[cfg(windows)]
fn system_get() -> Option<Result<Got, String>> {
    Some(windows::get())
}

/// The largest copy sent through the terminal: the protocol has every
/// terminal take 64 MB, and base64 makes it a third bigger on the way.
const TERMINAL_MAX: u64 = 48 * 1024 * 1024;

/// `None` when the terminal can't take it: no terminal, or one that
/// doesn't speak kitty's clipboard protocol.
#[cfg(unix)]
fn put_through_terminal(put: &Put) -> Option<Result<Copied, String>> {
    let mut tty = terminal::Tty::open()?;
    if !terminal_supported(&mut tty) {
        return None;
    }
    let items: Vec<(&str, Vec<u8>)> = match put {
        Put::Files(_) => {
            return Some(Err(
                "the terminal can take a picture or text, not files".to_string()
            ))
        }
        Put::Text { text, html, .. } => {
            let mut items = vec![("text/plain", text.clone().into_bytes())];
            if *html {
                items.push(("text/html", text.clone().into_bytes()));
            }
            items
        }
        Put::Picture { file, format } => {
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            if size > TERMINAL_MAX {
                return Some(Err(format!(
                    "{} is too big to send through the terminal",
                    file.display()
                )));
            }
            match std::fs::read(file) {
                Ok(bytes) => vec![(mime_of(*format).unwrap_or("image/png"), bytes)],
                Err(e) => return Some(Err(format!("couldn't read {}: {e}", file.display()))),
            }
        }
    };
    let items: Vec<(&str, &[u8])> = items.iter().map(|(m, b)| (*m, b.as_slice())).collect();
    if !tty.send(&terminal::write_escapes(&items)) {
        return None;
    }
    let answer = tty.answer(Duration::from_secs(5), |buf| {
        terminal::write_result(&terminal::take_packets(buf))
    });
    Some(match answer {
        Some(Ok(())) => Ok(Copied::Terminal { confirmed: true }),
        Some(Err(code)) => Err(terminal::explain(&code)),
        None => Ok(Copied::Terminal { confirmed: false }),
    })
}

#[cfg(unix)]
fn terminal_supported(tty: &mut terminal::Tty) -> bool {
    tty.send(terminal::SUPPORT_QUERY)
        && tty
            .answer(Duration::from_secs(1), |buf| terminal::supported_from(buf))
            .unwrap_or(false)
}

#[cfg(unix)]
fn get_through_terminal() -> Option<Result<Got, String>> {
    let mut tty = terminal::Tty::open()?;
    if !terminal_supported(&mut tty) {
        return None;
    }
    eprintln!("asking the terminal for the clipboard; allow it there if it asks");
    let mut read = |mimes: &[&str]| -> Result<Vec<(String, Vec<u8>)>, String> {
        if !tty.send(&terminal::read_escape(mimes)) {
            return Err("couldn't write to the terminal".to_string());
        }
        let mut got = Vec::new();
        let answer = tty.answer(
            Duration::from_secs(60),
            |buf| match terminal::read_progress(&mut got, &terminal::take_packets(buf)) {
                terminal::Read::Pending => None,
                done => Some(done),
            },
        );
        match answer {
            Some(terminal::Read::Done(items)) => Ok(items),
            Some(terminal::Read::Refused(code)) => Err(terminal::explain(&code)),
            _ => Err("the terminal didn't answer".to_string()),
        }
    };
    let result = (|| {
        let types = terminal::types_listed(&read(&["."])?);
        let want = if let Some((_, mime)) = best_picture(&types, |p| p.1) {
            mime.to_string()
        } else if types.iter().any(|t| t == "text/uri-list") {
            "text/uri-list".to_string()
        } else if types.iter().any(|t| t.starts_with("text/plain")) {
            "text/plain".to_string()
        } else {
            return Err("the clipboard holds no picture, files or text".to_string());
        };
        let items = read(&[want.as_str()])?;
        let bytes = items
            .into_iter()
            .find(|(m, _)| *m == want)
            .map(|(_, b)| b)
            .unwrap_or_default();
        Ok(got_from(&want, bytes))
    })();
    Some(result)
}

#[cfg(not(unix))]
fn put_through_terminal(_: &Put) -> Option<Result<Copied, String>> {
    None
}

#[cfg(not(unix))]
fn get_through_terminal() -> Option<Result<Got, String>> {
    None
}

/// What data of type `mime` is: a picture, a list of files, or text.
pub fn got_from(mime: &str, bytes: Vec<u8>) -> Got {
    if let Some(p) = PICTURES.iter().find(|p| p.1.eq_ignore_ascii_case(mime)) {
        return Got::Picture { format: p.0, bytes };
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if mime == "text/uri-list" || mime == "x-special/gnome-copied-files" {
        let files: Vec<PathBuf> = text.lines().filter_map(path_of_uri).collect();
        if !files.is_empty() {
            return Got::Files(files);
        }
    }
    Got::Text(text)
}

/// Text that is one existing file a line (paths or file:// links), as
/// files: a path copied from a terminal or an editor.
pub fn files_in_text(text: &str) -> Option<Vec<PathBuf>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let files: Vec<PathBuf> = lines
        .iter()
        .map(|l| {
            path_of_uri(l).unwrap_or_else(|| {
                let unquoted = l.trim_matches(|c| c == '\'' || c == '"');
                match unquoted.strip_prefix("~/") {
                    Some(rest) => std::env::var_os("HOME")
                        .map(|h| PathBuf::from(h).join(rest))
                        .unwrap_or_else(|| PathBuf::from(unquoted)),
                    None => PathBuf::from(unquoted),
                }
            })
        })
        .collect();
    files.iter().all(|f| f.is_file()).then_some(files)
}

/// `file:///a%20b.png` -> `/a b.png`.
pub fn path_of_uri(line: &str) -> Option<PathBuf> {
    let rest = line.trim().strip_prefix("file://")?;
    // `file://host/path`: only this computer's own files are files here.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(at) if &rest[..at] == "localhost" => &rest[at..],
        _ => return None,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if let (b'%', Some(&hi), Some(&lo)) = (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            if let (Some(hi), Some(lo)) = (hex(hi), hex(lo)) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8(out).ok()?;
    // `/C:/Users/...` on Windows.
    if cfg!(windows) && path.len() > 2 && path.as_bytes()[2] == b':' {
        return Some(PathBuf::from(&path[1..]));
    }
    Some(PathBuf::from(path))
}

/// `/a b.png` -> `file:///a%20b.png`.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub fn uri_of_path(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut s = String::from("file://");
    let text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        s.push('/');
    }
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// What to say after `--copy`, as `copied ...`.
pub fn copied_note(put: &Put, how: &Copied) -> String {
    let name = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let what = match put {
        Put::Text { file, .. } => format!("the text of {}", name(file)),
        Put::Picture { file, .. } => name(file),
        Put::Files(files) if files.len() == 1 => name(&files[0]),
        Put::Files(files) => format!("{} files", files.len()),
    };
    let mut s = format!("copied {what} to the clipboard");
    match how {
        Copied::Terminal { confirmed: true } => s.push_str(" (through the terminal)"),
        Copied::Terminal { confirmed: false } | Copied::Osc52 => {
            s.push_str("; asked the terminal to, and it didn't say whether it did")
        }
        Copied::Tool(_) => {}
    }
    s
}

/// Puts `text` on the clipboard: the system's own way first, then the
/// terminal's, which also works over SSH.
pub fn copy(text: &str) -> Result<Copied, String> {
    #[cfg(windows)]
    if windows::copy(text) {
        return Ok(Copied::Tool("the Windows clipboard"));
    }
    for (program, args) in tools() {
        if pipe_into(program, args, text) {
            return Ok(Copied::Tool(program));
        }
    }
    if std::io::stderr().is_terminal() {
        eprint!("{}", osc52(text));
        return Ok(Copied::Osc52);
    }
    Err("no clipboard tool found (pbcopy, wl-copy, xclip or xsel)".to_string())
}

/// Programs that take text on stdin and put it on the clipboard.
fn tools() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        return vec![("pbcopy", &[])];
    }
    if cfg!(windows) {
        return vec![];
    }
    let mut tools: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tools.push(("wl-copy", &[]));
    }
    tools.push(("xclip", &["-selection", "clipboard"]));
    tools.push(("xsel", &["--clipboard", "--input"]));
    tools
}

fn pipe_into(program: &str, args: &[&str], text: &str) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // pbcopy reads stdin in the locale's encoding, and a Quick Action or
    // a bare environment has none: braille would come out as mojibake.
    if program == "pbcopy" {
        cmd.env("LANG", "en_US.UTF-8");
    }
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

/// The escape that asks the terminal to put `text` on the clipboard.
pub fn osc52(text: &str) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07")
}

/// What to tell the user after copying `chars` characters: where pasting
/// keeps it lined up, and a width that fits a Discord message when this
/// doesn't. `fits` is that width.
pub fn note(chars: usize, fits: u32, how: &Copied) -> String {
    let mut s = format!(
        "copied {chars} characters; in Discord, Slack or GitHub, paste it inside ``` so it stays lined up"
    );
    // Eight more for the ``` fences and their newlines.
    if chars + 8 > 2000 {
        s.push_str(&format!(
            "; that's over Discord's 2,000 a message, and --width {fits} would fit"
        ));
    }
    if *how == Copied::Osc52 {
        s.push_str("; asked the terminal to copy it, which some terminals don't allow");
    }
    s
}

/// `note` in one row of 80 or fewer, for `conv ascii -i`'s status row:
/// the Discord limit first when it is passed, with the width that fits.
pub fn short_note(chars: usize, fits: u32, how: &Copied) -> String {
    let mut s = if chars + 8 > 2000 {
        format!("copied {chars} characters, over Discord's 2,000; {fits} columns would fit")
    } else {
        format!("copied {chars} characters; paste it inside ``` to keep it lined up")
    };
    // That the terminal was asked matters less than the limit: only when
    // there is room.
    const VIA: &str = " (via the terminal)";
    if *how == Copied::Osc52 && s.chars().count() + VIA.len() <= 80 {
        s.push_str(VIA);
    }
    s
}

/// The Windows clipboard: text as UTF-16 (`clip.exe` would mangle UTF-8),
/// files as a file drop, pictures as PNG.
#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};

    use super::{files_in_text, Got, Put};
    use convkit_core::Format;

    fn png_format() -> u32 {
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        // SAFETY: a NUL-terminated UTF-16 string.
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    /// A DROPFILES header and the paths after it, as Explorer puts them.
    fn file_drop(files: &[PathBuf]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&20u32.to_le_bytes()); // where the paths start
        bytes.extend_from_slice(&[0u8; 8]); // the drop point
        bytes.extend_from_slice(&0i32.to_le_bytes()); // not in a non-client area
        bytes.extend_from_slice(&1i32.to_le_bytes()); // UTF-16
        for f in files {
            let f = std::path::absolute(f).unwrap_or_else(|_| f.clone());
            for unit in f.as_os_str().encode_wide().chain(std::iter::once(0)) {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    fn utf16_units(bytes: &[u8]) -> Vec<u16> {
        (0..bytes.len() / 2)
            .map(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
            .collect()
    }

    fn files_of_drop(bytes: &[u8]) -> Vec<PathBuf> {
        let Some(start) = bytes
            .get(..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        else {
            return Vec::new();
        };
        let wide = bytes.get(16..20).is_some_and(|b| b != [0, 0, 0, 0]);
        let rest = bytes.get(start as usize..).unwrap_or_default();
        let names: Vec<String> = if wide {
            let units = utf16_units(rest);
            units
                .split(|u| *u == 0)
                .take_while(|n| !n.is_empty())
                .map(String::from_utf16_lossy)
                .collect()
        } else {
            rest.split(|b| *b == 0)
                .take_while(|n| !n.is_empty())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect()
        };
        names.into_iter().map(PathBuf::from).collect()
    }

    /// A BMP file from a CF_DIB: the 14-byte file header the clipboard
    /// leaves off, then the DIB.
    fn bmp_of_dib(dib: &[u8]) -> Option<Vec<u8>> {
        let header = u32::from_le_bytes(dib.get(..4)?.try_into().ok()?) as usize;
        let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?) as usize;
        let compression = u32::from_le_bytes(dib.get(16..20)?.try_into().ok()?);
        let used = u32::from_le_bytes(dib.get(32..36)?.try_into().ok()?) as usize;
        let colours = if used > 0 {
            used
        } else if bits <= 8 {
            1 << bits
        } else {
            0
        };
        // BI_BITFIELDS with a plain header: three masks follow it.
        let masks = if compression == 3 && header == 40 {
            12
        } else {
            0
        };
        let offset = 14 + header + masks + colours * 4;
        let mut bmp = Vec::with_capacity(14 + dib.len());
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&(offset as u32).to_le_bytes());
        bmp.extend_from_slice(dib);
        Some(bmp)
    }

    /// SAFETY: the clipboard must be open.
    unsafe fn set(format: u32, bytes: &[u8]) -> bool {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if mem.is_null() {
            return false;
        }
        let at = GlobalLock(mem) as *mut u8;
        if at.is_null() {
            GlobalFree(mem);
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        GlobalUnlock(mem);
        if SetClipboardData(format, mem).is_null() {
            GlobalFree(mem);
            return false;
        }
        true
    }

    /// SAFETY: the clipboard must be open.
    unsafe fn data(format: u32) -> Option<Vec<u8>> {
        if IsClipboardFormatAvailable(format) == 0 {
            return None;
        }
        let mem = GetClipboardData(format);
        if mem.is_null() {
            return None;
        }
        let len = GlobalSize(mem);
        let at = GlobalLock(mem) as *const u8;
        if at.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(at, len).to_vec();
        GlobalUnlock(mem);
        Some(bytes)
    }

    pub fn put(put: &Put) -> Result<(), String> {
        let mut sets: Vec<(u32, Vec<u8>)> = Vec::new();
        match put {
            Put::Text { text, .. } => {
                let wide: Vec<u8> = text
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                sets.push((u32::from(CF_UNICODETEXT), wide));
            }
            Put::Picture { file, format } => {
                sets.push((u32::from(CF_HDROP), file_drop(std::slice::from_ref(file))));
                if *format == Format::Png {
                    let bytes = std::fs::read(file)
                        .map_err(|e| format!("couldn't read {}: {e}", file.display()))?;
                    sets.push((png_format(), bytes));
                }
            }
            Put::Files(files) => sets.push((u32::from(CF_HDROP), file_drop(files))),
        }
        // SAFETY: the clipboard is opened and closed here.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("the clipboard is in use".to_string());
            }
            let ok = EmptyClipboard() != 0 && sets.iter().all(|(f, b)| set(*f, b));
            CloseClipboard();
            if ok {
                Ok(())
            } else {
                Err("the clipboard didn't take it".to_string())
            }
        }
    }

    pub fn get() -> Result<Got, String> {
        // SAFETY: the clipboard is opened and closed here.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("the clipboard is in use".to_string());
            }
            let got = (|| {
                if let Some(drop) = data(u32::from(CF_HDROP)) {
                    let files = files_of_drop(&drop);
                    if !files.is_empty() {
                        return Ok(Got::Files(files));
                    }
                }
                if let Some(bytes) = data(png_format()) {
                    return Ok(Got::Picture {
                        format: Format::Png,
                        bytes,
                    });
                }
                if let Some(bmp) = data(u32::from(CF_DIB)).and_then(|d| bmp_of_dib(&d)) {
                    return Ok(Got::Picture {
                        format: Format::Bmp,
                        bytes: bmp,
                    });
                }
                if let Some(wide) = data(u32::from(CF_UNICODETEXT)) {
                    let units: Vec<u16> = utf16_units(&wide)
                        .into_iter()
                        .take_while(|u| *u != 0)
                        .collect();
                    let text = String::from_utf16_lossy(&units);
                    return Ok(match files_in_text(&text) {
                        Some(files) => Got::Files(files),
                        None => Got::Text(text),
                    });
                }
                Err("the clipboard is empty".to_string())
            })();
            CloseClipboard();
            got
        }
    }

    pub fn copy(text: &str) -> bool {
        put(&Put::Text {
            file: PathBuf::new(),
            text: text.to_string(),
            html: false,
        })
        .is_ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_file_drop_reads_back() {
            let files = vec![PathBuf::from("C:\\a b\\x.png"), PathBuf::from("C:\\y.jpg")];
            assert_eq!(files_of_drop(&file_drop(&files)), files);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_picture_is_copied_as_a_picture_and_text_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let md = dir.path().join("notes.md");
        let mp4 = dir.path().join("clip.mp4");
        std::fs::write(&png, b"x").unwrap();
        std::fs::write(&md, "# hi").unwrap();
        std::fs::write(&mp4, b"x").unwrap();
        assert_eq!(
            put_for(std::slice::from_ref(&png)).unwrap(),
            Put::Picture {
                file: png.clone(),
                format: Format::Png
            }
        );
        assert_eq!(
            put_for(std::slice::from_ref(&md)).unwrap(),
            Put::Text {
                file: md.clone(),
                text: "# hi".to_string(),
                html: false
            }
        );
        assert_eq!(
            put_for(std::slice::from_ref(&mp4)).unwrap(),
            Put::Files(vec![mp4.clone()])
        );
        // Several outputs are files, even when they are pictures.
        assert_eq!(
            put_for(&[png.clone(), md.clone()]).unwrap(),
            Put::Files(vec![png, md])
        );
    }

    #[test]
    fn file_links_and_paths_go_both_ways() {
        assert_eq!(
            path_of_uri("file:///tmp/a%20b%C3%A9.png"),
            Some(PathBuf::from("/tmp/a bé.png"))
        );
        assert_eq!(
            path_of_uri("file://localhost/tmp/x"),
            Some(PathBuf::from("/tmp/x"))
        );
        // Another computer's file is not a file here; a link is not a path.
        assert_eq!(path_of_uri("file://server/share/x"), None);
        assert_eq!(path_of_uri("https://example.com/a.png"), None);
        assert_eq!(
            path_of_uri("file:///tmp/100%"),
            Some(PathBuf::from("/tmp/100%"))
        );
        if cfg!(unix) {
            let p = Path::new("/tmp/a b#é.png");
            assert_eq!(uri_of_path(p), "file:///tmp/a%20b%23%C3%A9.png");
            assert_eq!(path_of_uri(&uri_of_path(p)), Some(p.to_path_buf()));
        }
    }

    #[test]
    fn text_is_files_only_when_every_line_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a b.png");
        std::fs::write(&a, b"x").unwrap();
        let quoted = format!("'{}'\n", a.display());
        assert_eq!(files_in_text(&quoted), Some(vec![a.clone()]));
        let link = format!("{}\r\n", uri_of_path(&a));
        assert_eq!(files_in_text(&link), Some(vec![a.clone()]));
        let mixed = format!("{}\nhello", a.display());
        assert_eq!(files_in_text(&mixed), None);
        assert_eq!(files_in_text("  \n"), None);
    }

    #[test]
    fn what_comes_off_the_clipboard_is_told_by_its_type() {
        assert_eq!(
            got_from("image/jpeg", vec![1, 2]),
            Got::Picture {
                format: Format::Jpg,
                bytes: vec![1, 2]
            }
        );
        let dir = tempfile::tempdir().unwrap();
        let list = format!("# a comment\r\n{}\r\n", uri_of_path(dir.path()));
        assert_eq!(
            got_from("text/uri-list", list.into_bytes()),
            Got::Files(vec![dir.path().to_path_buf()])
        );
        assert_eq!(
            got_from("text/plain", b"hi".to_vec()),
            Got::Text("hi".to_string())
        );
    }

    #[test]
    fn the_most_wanted_picture_is_taken() {
        let types: Vec<String> = ["public.tiff", "public.png", "public.utf8-plain-text"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            best_picture(&types, |p| p.2),
            Some((Format::Png, "public.png"))
        );
        assert_eq!(best_picture(&types[2..], |p| p.2), None);
    }

    #[test]
    fn the_note_says_what_was_copied_and_how() {
        let put = Put::Picture {
            file: PathBuf::from("out/a.jpg"),
            format: Format::Jpg,
        };
        assert_eq!(
            copied_note(&put, &Copied::Tool("x")),
            "copied a.jpg to the clipboard"
        );
        assert!(copied_note(&put, &Copied::Terminal { confirmed: true })
            .ends_with("(through the terminal)"));
        assert!(copied_note(&put, &Copied::Terminal { confirmed: false }).contains("didn't say"));
        let files = Put::Files(vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(
            copied_note(&files, &Copied::Tool("x")),
            "copied 2 files to the clipboard"
        );
    }

    #[test]
    fn osc52_wraps_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn a_short_note_leads_with_the_limit_and_fits_a_row() {
        // The tuner has one row for it: the warning comes first.
        let long = short_note(2500, 60, &Copied::Osc52);
        assert!(
            long.starts_with("copied 2500 characters, over Discord's 2,000"),
            "{long}"
        );
        assert!(long.contains("60 columns"), "{long}");
        let short = short_note(1500, 60, &Copied::Tool("pbcopy"));
        assert!(short.contains("```") && !short.contains("2,000"), "{short}");
        for note in [long, short, short_note(10, 60, &Copied::Osc52)] {
            assert!(note.chars().count() <= 80, "{note}");
        }
    }

    #[test]
    fn note_mentions_backticks_and_the_limit() {
        let short = note(1500, 60, &Copied::Tool("pbcopy"));
        assert!(short.contains("```") && !short.contains("2,000"), "{short}");
        let long = note(2500, 60, &Copied::Tool("pbcopy"));
        assert!(
            long.contains("2,000") && long.contains("--width 60"),
            "{long}"
        );
        let osc = note(10, 60, &Copied::Osc52);
        assert!(osc.contains("asked the terminal"), "{osc}");
    }
}
