//! Copying text to the clipboard, for `conv ascii --clipboard`.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

use base64::Engine;

/// How the text was copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// By this program or API, which says whether it worked.
    Tool(&'static str),
    /// By asking the terminal (OSC 52), which can't say whether it did.
    Osc52,
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

/// The Windows clipboard, as UTF-16: `clip.exe` would mangle UTF-8.
#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    pub fn copy(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the clipboard is opened and closed here; the memory is
        // allocated movable as SetClipboardData requires, filled while
        // locked, and freed here only when the clipboard didn't take it.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return false;
            }
            let ok = put(&wide);
            CloseClipboard();
            ok
        }
    }

    /// SAFETY: the clipboard must be open.
    unsafe fn put(wide: &[u16]) -> bool {
        if EmptyClipboard() == 0 {
            return false;
        }
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if mem.is_null() {
            return false;
        }
        let at = GlobalLock(mem) as *mut u16;
        if at.is_null() {
            GlobalFree(mem);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), at, wide.len());
        GlobalUnlock(mem);
        if SetClipboardData(u32::from(CF_UNICODETEXT), mem).is_null() {
            GlobalFree(mem);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
