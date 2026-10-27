//! The clipboard on Linux and the BSDs, through wl-clipboard on Wayland or
//! xclip on X11. Both carry any type; xsel, text only, is left to
//! `conv ascii --clipboard`.

use std::io::Write;
use std::process::{Command, Stdio};

use super::{best_picture, files_in_text, got_from, mime_of, uri_of_path, Got, Put};

/// The tool here, if any: `wl` for wl-clipboard, else xclip.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Wayland,
    X11,
}

fn tool() -> Option<Tool> {
    let on = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    let has = |program: &str| {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    if on("WAYLAND_DISPLAY") && has("wl-copy") {
        return Some(Tool::Wayland);
    }
    if on("DISPLAY") && has("xclip") {
        return Some(Tool::X11);
    }
    None
}

/// `None` when there is no clipboard tool to use.
pub fn put(put: &Put) -> Option<Result<&'static str, String>> {
    let tool = tool()?;
    let (mime, bytes) = match put {
        Put::Text { text, .. } => ("text/plain;charset=utf-8", text.clone().into_bytes()),
        Put::Picture { file, format } => match std::fs::read(file) {
            Ok(bytes) => (mime_of(*format).unwrap_or("image/png"), bytes),
            Err(e) => return Some(Err(format!("couldn't read {}: {e}", file.display()))),
        },
        Put::Files(files) => {
            let list: Vec<String> = files.iter().map(|f| uri_of_path(f)).collect();
            ("text/uri-list", (list.join("\r\n") + "\r\n").into_bytes())
        }
    };
    let (program, args): (&'static str, Vec<&str>) = match tool {
        Tool::Wayland => ("wl-copy", vec!["--type", mime]),
        Tool::X11 => (
            "xclip",
            vec!["-selection", "clipboard", "-target", mime, "-in"],
        ),
    };
    let mut child = match Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Some(Err(format!("couldn't run {program}: {e}"))),
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(&bytes).is_ok());
    // Both stay behind to serve the clipboard; they return once they have
    // taken it.
    let ok = child.wait().is_ok_and(|s| s.success()) && wrote;
    Some(if ok {
        Ok(program)
    } else {
        Err(format!("{program} didn't take it"))
    })
}

fn read(tool: Tool, mime: &str) -> Option<Vec<u8>> {
    let out = match tool {
        Tool::Wayland => Command::new("wl-paste")
            .args(["--no-newline", "--type", mime])
            .output(),
        Tool::X11 => Command::new("xclip")
            .args(["-selection", "clipboard", "-target", mime, "-out"])
            .output(),
    }
    .ok()?;
    out.status.success().then_some(out.stdout)
}

fn types(tool: Tool) -> Vec<String> {
    let out = match tool {
        Tool::Wayland => Command::new("wl-paste").arg("--list-types").output(),
        Tool::X11 => Command::new("xclip")
            .args(["-selection", "clipboard", "-target", "TARGETS", "-out"])
            .output(),
    };
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// `None` when there is no clipboard tool to use.
pub fn get() -> Option<Result<Got, String>> {
    let tool = tool()?;
    let types = types(tool);
    for list in ["text/uri-list", "x-special/gnome-copied-files"] {
        if types.iter().any(|t| t == list) {
            if let Some(bytes) = read(tool, list) {
                if let got @ Got::Files(_) = got_from(list, bytes) {
                    return Some(Ok(got));
                }
            }
        }
    }
    if let Some((_, mime)) = best_picture(&types, |p| p.1) {
        if let Some(bytes) = read(tool, mime) {
            return Some(Ok(got_from(mime, bytes)));
        }
    }
    let text = types
        .iter()
        .find(|t| t.starts_with("text/plain") || *t == "UTF8_STRING" || *t == "STRING")
        .and_then(|t| read(tool, t));
    Some(match text {
        Some(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            Ok(match files_in_text(&text) {
                Some(files) => Got::Files(files),
                None => Got::Text(text),
            })
        }
        None => Err("the clipboard is empty".to_string()),
    })
}
