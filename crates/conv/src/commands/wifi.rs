//! `conv wifi [NAME]`: a saved Wi-Fi network's security and password, or
//! with `--qr` a code that joins it. `conv qr --wifi` makes the same code
//! through `network`.
//!
//! The password is shown only where a person reads it: on a terminal. Into
//! a pipe, a file or `--json` it goes only with `--show-password`, so a
//! script or an agent that runs conv doesn't write it into a log without
//! being told to; `--copy` and `--qr` hand it over without printing it.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::qr::DEFAULT_SIZE;
use convkit_core::wifi::{self, Network, Platform, Security};
use convkit_core::{ConvError, ErrorCode};
use serde_json::json;

use crate::cli::{Cli, Command};
use crate::commands::{convert, qr};
use crate::{clipboard, prompt, render};

pub fn run(cli: &Cli) -> i32 {
    match show(cli) {
        Ok(()) => 0,
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

fn platform() -> Result<Platform, ConvError> {
    Platform::current().ok_or_else(|| {
        ConvError::new(
            ErrorCode::UnsupportedPair,
            "conv wifi reads saved networks on macOS, Windows and Linux",
        )
    })
}

fn show(cli: &Cli) -> Result<(), ConvError> {
    let Some(Command::Wifi {
        name,
        list,
        qr: qr_out,
        outdir,
        overwrite,
        copy,
        show_password,
        dry_run,
    }) = &cli.command
    else {
        unreachable!("main dispatches only Command::Wifi here");
    };
    let platform = platform()?;
    if *list {
        let saved = wifi::saved(platform, &wifi::run_system)?;
        if cli.json {
            let v = json!({ "ok": true, "networks": saved });
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else {
            for n in &saved {
                println!("{}", render::safe_for_terminal(n));
            }
        }
        return Ok(());
    }
    // `--qr` alone is "" here: the name it gets comes from the network.
    let qr_out: Option<Option<&Path>> = qr_out
        .as_deref()
        .map(|p| (!p.as_os_str().is_empty()).then_some(p));
    // A file name without an extension is more likely the network's name
    // typed after --qr; say so before anything is asked.
    if let Some(Some(out)) = qr_out {
        if out.extension().is_none() && !crate::input::is_extension_shorthand(out) {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "--qr takes the file to write, and {} has no extension; to name the network, put it before --qr: conv wifi {} --qr",
                    out.display(),
                    render::shell_quote(&out.to_string_lossy())
                ),
            ));
        }
        qr::output_path(Some(out), outdir.as_deref(), "wifi")?;
    }
    let destination = |name: &str| -> Result<Option<(PathBuf, convkit_core::Format)>, ConvError> {
        let Some(out) = qr_out else { return Ok(None) };
        let (path, format) = qr::output_path(out, outdir.as_deref(), &qr_stem(name))?;
        if path.exists() && !overwrite {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} exists; pass -y to overwrite", path.display()),
            ));
        }
        Ok(Some((path, format)))
    };
    if *dry_run {
        let name = chosen(cli, platform, name.as_deref())?;
        let target = destination(&name)?;
        let cmd = wifi::password_command(platform, &name)?;
        if cli.json {
            let mut v =
                json!({ "ok": true, "dry_run": true, "network": name, "command": cmd.display() });
            if let Some((path, _)) = &target {
                v["output"] = json!(path.to_string_lossy());
            }
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else {
            println!("{}", render::safe_for_terminal(&cmd.display()));
            if let Some((path, _)) = &target {
                let path = render::safe_for_terminal(&path.display().to_string());
                println!("would write {path}");
            }
        }
        return Ok(());
    }
    let start = Instant::now();
    let (network, target) = network(cli, name.as_deref(), destination)?;
    if let Some((path, format)) = target {
        let text = network.join_text()?;
        let (mut outcome, code) = qr::write(
            cli,
            &text,
            &path,
            format,
            outdir.as_deref(),
            None,
            DEFAULT_SIZE,
            *overwrite,
        )?;
        outcome.elapsed_ms = start.elapsed().as_millis() as u64;
        let mut notes = network.notes.clone();
        notes.append(&mut outcome.warnings);
        outcome.warnings = notes;
        let copied = copy.then(|| convert::copy_outputs(std::slice::from_ref(&outcome.output)));
        qr::print(
            cli,
            &outcome,
            qr::Shown::Wifi(&network),
            &code,
            copied.as_ref(),
        );
        return Ok(());
    }
    let reveal = shows_password(*show_password, cli.json, std::io::stdout().is_terminal());
    let copied = copy.then(|| copy_password(&network));
    if cli.json {
        let mut v = json!({
            "ok": true,
            "network": network_json(&network, reveal),
            "password_shown": reveal,
            "notes": network.notes,
        });
        if let Some((ok, message)) = &copied {
            v["clipboard"] = json!({ "ok": ok, "message": message });
        }
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", human(&network, reveal));
        let mut notes = network.notes.clone();
        if !reveal && network.password.is_some() && !copy {
            notes.push(
                "the password isn't printed into a pipe or a file; --show-password prints it, --copy copies it"
                    .to_string(),
            );
        }
        if !cli.quiet {
            eprint!("{}", render::notes_human(&notes, render::stderr_styled()));
        }
    }
    if let (false, Some((ok, message))) = (cli.json, copied) {
        if !ok {
            let line = format!("warning  {message}");
            eprintln!("{}", render::paint_warning(&line, render::stderr_styled()));
        } else if !cli.quiet {
            eprintln!("{}", render::safe_for_terminal(&message));
        }
    }
    Ok(())
}

/// Whether the password is printed: when asked, or on a terminal, but
/// never into `--json` unasked, as the help says.
fn shows_password(asked: bool, json: bool, terminal: bool) -> bool {
    asked || (!json && terminal)
}

/// Finds the network to read (NAME, the one this computer is on, or the
/// one picked from the saved list), lets `before` check what will be
/// written while nothing has been asked yet, says when the system is about
/// to ask, and reads it.
pub(crate) fn network<T>(
    cli: &Cli,
    name: Option<&str>,
    before: impl FnOnce(&str) -> Result<T, ConvError>,
) -> Result<(Network, T), ConvError> {
    let platform = platform()?;
    let name = chosen(cli, platform, name)?;
    let checked = before(&name)?;
    // A name the Mac has never joined has no password to ask for, and the
    // keychain says so without a prompt.
    let saved = || {
        wifi::saved(platform, &wifi::run_system)
            .map(|s| s.contains(&name))
            .unwrap_or(true)
    };
    if platform.asks_first() && !cli.quiet && saved() {
        eprint!(
            "{}",
            render::notes_human(
                &[format!(
                    "macOS will ask for your password to share the Wi-Fi password saved for {name}"
                )],
                render::stderr_styled()
            )
        );
    }
    let network = wifi::read(platform, &name, &wifi::run_system)?;
    Ok((network, checked))
}

/// NAME as given, or the network this computer is on, or, when the system
/// won't say, the one picked from the saved list.
fn chosen(cli: &Cli, platform: Platform, name: Option<&str>) -> Result<String, ConvError> {
    if let Some(n) = name {
        if n.is_empty() {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "the network's name is empty",
            ));
        }
        return Ok(n.to_string());
    }
    if let Some(n) = wifi::current(platform, &wifi::run_system)? {
        if !cli.quiet {
            eprint!(
                "{}",
                render::notes_human(
                    &[format!("{n} is the network this computer is on")],
                    render::stderr_styled()
                )
            );
        }
        return Ok(n);
    }
    let saved = wifi::saved(platform, &wifi::run_system)?;
    let why = if platform == Platform::Mac {
        "macOS doesn't tell programs without Location access which network it is on"
    } else {
        "conv can't tell which network this computer is on"
    };
    if saved.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("{why}, and there are no saved networks to pick from"),
        ));
    }
    if prompt::is_interactive_session() && !cli.json {
        eprintln!("{why}. Saved networks:");
        eprint!("{}", numbered(&saved));
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "Which one? Type its number or name: ");
        let _ = stderr.flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return pick(&saved, line.trim_end_matches(['\r', '\n']))
            .ok_or_else(|| ConvError::new(ErrorCode::InvalidInvocation, "no network was picked"));
    }
    let message = if cli.json {
        format!(
            "{why}; name one of the saved networks: {}",
            saved.join(", ")
        )
    } else {
        eprintln!("Saved networks:");
        eprint!("{}", numbered(&saved));
        format!("{why}; name one of the saved networks above")
    };
    Err(ConvError {
        code: ErrorCode::InvalidInvocation,
        message,
        backend: None,
        remediation: Some(convkit_core::Remediation {
            managed: None,
            manual: Some(format!("conv wifi {}", render::shell_quote(&saved[0]))),
        }),
    })
}

/// The saved networks, numbered from 1.
fn numbered(saved: &[String]) -> String {
    let w = saved.len().to_string().len();
    saved
        .iter()
        .enumerate()
        .map(|(i, n)| format!("  {:>w$}  {}\n", i + 1, render::safe_for_terminal(n)))
        .collect()
}

/// The network an answer names: its number, its name, or its name in any
/// case when only one matches.
fn pick(saved: &[String], answer: &str) -> Option<String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return None;
    }
    if let Ok(i) = answer.parse::<usize>() {
        if (1..=saved.len()).contains(&i) {
            return Some(saved[i - 1].clone());
        }
    }
    if let Some(n) = saved.iter().find(|n| n.as_str() == answer) {
        return Some(n.clone());
    }
    let lower = answer.to_lowercase();
    let mut matches = saved.iter().filter(|n| n.to_lowercase() == lower);
    match (matches.next(), matches.next()) {
        (Some(n), None) => Some(n.clone()),
        _ => None,
    }
}

/// The start of a code's file name: the network's name, with what a file
/// name can't hold turned into dashes, then `-wifi`.
pub(crate) fn qr_stem(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if clean.is_empty() {
        "wifi".to_string()
    } else {
        format!("{clean}-wifi")
    }
}

/// The network for `--json`, with its password only when `with_password`.
pub(crate) fn network_json(n: &Network, with_password: bool) -> serde_json::Value {
    let kind = match n.security {
        Security::Open => "open",
        Security::Wep => "wep",
        Security::Wpa => "wpa",
        Security::Enterprise => "enterprise",
    };
    let mut v = json!({
        "name": n.name,
        "ssid": n.ssid,
        "security": n.security_text(),
        "kind": kind,
        "hidden": n.hidden,
    });
    if with_password {
        v["password"] = json!(n.password);
    }
    v
}

/// The network as labelled lines, the password among them only when
/// `reveal`.
fn human(n: &Network, reveal: bool) -> String {
    let mut rows = vec![("network", n.name.clone())];
    if n.ssid != n.name {
        rows.push(("ssid", n.ssid.clone()));
    }
    rows.push(("security", n.security_text()));
    if n.hidden {
        rows.push(("hidden", "yes, it doesn't announce its name".to_string()));
    }
    match (&n.password, n.security) {
        (Some(p), _) if reveal => rows.push(("password", p.clone())),
        (None, Security::Open) => rows.push(("password", "none, it's open".to_string())),
        _ => {}
    }
    // Names and passwords are whatever was saved, escapes and all.
    rows.iter()
        .map(|(k, v)| format!("{k:<9} {}\n", render::safe_for_terminal(v)))
        .collect()
}

/// Copies the password and says so, never with the password in the note.
fn copy_password(n: &Network) -> (bool, String) {
    let Some(password) = &n.password else {
        return (
            false,
            format!("{} is open, so there's no password to copy", n.name),
        );
    };
    match clipboard::copy(password) {
        Ok(clipboard::Copied::Osc52) => (
            true,
            format!(
                "asked the terminal to copy the password for {}, which some terminals don't allow",
                n.name
            ),
        ),
        Ok(_) => (
            true,
            format!("copied the password for {} to the clipboard", n.name),
        ),
        Err(e) => (false, format!("couldn't copy to the clipboard: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(security: Security, password: Option<&str>) -> Network {
        Network {
            name: "Home".to_string(),
            ssid: "Home".to_string(),
            security,
            label: None,
            password: password.map(str::to_string),
            hidden: false,
            notes: vec![],
        }
    }

    #[test]
    fn the_password_shows_only_when_revealed() {
        let n = net(Security::Wpa, Some("hunter22"));
        assert_eq!(
            human(&n, true),
            "network   Home\nsecurity  WPA/WPA2/WPA3\npassword  hunter22\n"
        );
        let hidden = human(&n, false);
        assert!(!hidden.contains("hunter22"), "{hidden}");
        assert_eq!(hidden, "network   Home\nsecurity  WPA/WPA2/WPA3\n");
        let json = network_json(&n, false).to_string();
        assert!(!json.contains("hunter22"), "{json}");
        assert_eq!(network_json(&n, true)["password"], "hunter22");
    }

    #[test]
    fn json_leaves_the_password_out_unless_asked() {
        assert!(!shows_password(false, true, true));
        assert!(shows_password(true, true, true));
        assert!(shows_password(false, false, true));
        assert!(!shows_password(false, false, false));
    }

    #[test]
    fn a_saved_name_cannot_reach_the_terminal() {
        let mut n = net(Security::Wpa, Some("p\u{1b}[2J"));
        n.name = "a\u{1b}]52;c;aGk=\u{7}".to_string();
        let text = human(&n, true) + &numbered(&[n.name.clone()]);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }

    #[test]
    fn an_open_network_says_it_has_no_password() {
        let n = net(Security::Open, None);
        assert!(human(&n, false).contains("password  none, it's open\n"));
        let (ok, message) = copy_password(&n);
        assert!(!ok);
        assert!(message.contains("no password to copy"), "{message}");
    }

    #[test]
    fn a_code_is_named_after_its_network() {
        assert_eq!(qr_stem("Home"), "Home-wifi");
        assert_eq!(qr_stem("Café 5G"), "Café 5G-wifi");
        assert_eq!(qr_stem("a/b:c"), "a-b-c-wifi");
        assert_eq!(qr_stem("..."), "wifi");
        assert_eq!(qr_stem(" .hidden. "), "hidden-wifi");
    }

    #[test]
    fn a_network_is_picked_by_number_or_name() {
        let saved = vec![
            "Home".to_string(),
            "Office".to_string(),
            "office".to_string(),
        ];
        assert_eq!(pick(&saved, "1").as_deref(), Some("Home"));
        assert_eq!(pick(&saved, " 2 ").as_deref(), Some("Office"));
        assert_eq!(pick(&saved, "home").as_deref(), Some("Home"));
        assert_eq!(pick(&saved, "office").as_deref(), Some("office"));
        // Two match in another case: neither is guessed.
        assert_eq!(pick(&saved, "OFFICE"), None);
        assert_eq!(pick(&saved, "4"), None);
        assert_eq!(pick(&saved, ""), None);
    }

    #[test]
    fn saved_networks_are_numbered_in_a_column() {
        let saved: Vec<String> = (1..=10).map(|i| format!("n{i}")).collect();
        let text = numbered(&saved);
        assert!(text.starts_with("   1  n1\n"), "{text}");
        assert!(text.ends_with("  10  n10\n"), "{text}");
    }
}
