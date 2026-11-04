//! Saved Wi-Fi networks and their passwords, read from the system's own
//! store (`conv wifi`), and the text of a QR code that joins one.
//!
//! Each system keeps them its own way, so each is asked with its own tool:
//! the keychain on macOS (`security`, which shows macOS's own prompt
//! before it shares a password), `netsh` on Windows, and NetworkManager's
//! `nmcli` on Linux. Nothing here prints, and a password is never put in
//! an error, a command line or a `Debug` dump: it only leaves in the
//! `Network` it was read into.

use std::fmt;
use std::process::{Command, Stdio};

use crate::{ConvError, ErrorCode, Result};

/// macOS keeps a Wi-Fi password as a keychain item of this kind, under the
/// network's name.
const AIRPORT_KIND: &str = "AirPort network password";

/// Where the macOS tools live, named in full so whatever is on `PATH`
/// cannot stand in for them.
const SECURITY: &str = "/usr/bin/security";
const NETWORKSETUP: &str = "/usr/sbin/networksetup";
const IPCONFIG: &str = "/usr/sbin/ipconfig";

/// The system whose store is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
    Linux,
}

impl Platform {
    /// The system conv runs on, if it is one of the three.
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "macos") {
            Some(Platform::Mac)
        } else if cfg!(windows) {
            Some(Platform::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else {
            None
        }
    }

    /// Whether the system asks the person at the computer before it hands
    /// over a password.
    pub fn asks_first(&self) -> bool {
        *self == Platform::Mac
    }

    fn computer(&self) -> &'static str {
        match self {
            Platform::Mac => "this Mac",
            Platform::Windows | Platform::Linux => "this computer",
        }
    }
}

/// How a network is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// No password.
    Open,
    Wep,
    /// WPA, WPA2 or WPA3 with one shared password.
    Wpa,
    /// A sign-in for each person (802.1X), which no join code can hold.
    Enterprise,
}

/// A saved network.
#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    /// The name it was asked for by: the network's own name on macOS, the
    /// profile's or connection's name on Windows and Linux.
    pub name: String,
    /// The network's own name, which goes in the code.
    pub ssid: String,
    pub security: Security,
    /// The system's own name for the security, when it gives one
    /// (`WPA2-Personal`).
    pub label: Option<String>,
    pub password: Option<String>,
    /// The network doesn't announce itself, so a phone has to be told to
    /// look for it.
    pub hidden: bool,
    /// Something the reader should know about how this was read.
    pub notes: Vec<String>,
}

/// Never shows the password, so a stray `{:?}` can't leak it into a log.
impl fmt::Debug for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network")
            .field("name", &self.name)
            .field("ssid", &self.ssid)
            .field("security", &self.security)
            .field("label", &self.label)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("hidden", &self.hidden)
            .finish()
    }
}

impl Network {
    /// The security as shown to a person.
    pub fn security_text(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        match self.security {
            Security::Open => "open, no password",
            Security::Wep => "WEP",
            Security::Wpa => "WPA/WPA2/WPA3",
            Security::Enterprise => "WPA Enterprise",
        }
        .to_string()
    }

    /// The text of a QR code that joins this network, in the `WIFI:` form
    /// phone cameras read (`WIFI:T:WPA;S:name;P:password;;`).
    pub fn join_text(&self) -> Result<String> {
        let kind = match self.security {
            Security::Open => "nopass",
            Security::Wep => "WEP",
            Security::Wpa => "WPA",
            Security::Enterprise => {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "{} uses {}, where each person signs in with their own account, which a join code can't hold",
                        self.name,
                        self.security_text()
                    ),
                ))
            }
        };
        let mut text = format!("WIFI:T:{kind};S:{};", escape(&self.ssid));
        if self.security != Security::Open {
            let password = self.password.as_deref().ok_or_else(|| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("no password was read for {}", self.name),
                )
            })?;
            text.push_str(&format!("P:{};", escape(password)));
        }
        if self.hidden {
            text.push_str("H:true;");
        }
        text.push(';');
        Ok(text)
    }
}

/// A value in a `WIFI:` code: a backslash before each of `\ ; , " :`,
/// which the format uses to separate and quote its fields.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | ';' | ',' | '"' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A command conv runs to ask the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    /// On Windows these are passed exactly as written, quotes included,
    /// because netsh reads `name="My Network"` itself.
    pub args: Vec<String>,
}

impl Cmd {
    fn new(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// The command as it would be typed, for `--dry-run`.
    pub fn display(&self) -> String {
        let quote = |a: &str| {
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
            // On Windows the arguments are passed as written, quotes and
            // all, so they are shown that way too.
            if plain || cfg!(windows) || a.contains('"') {
                a.to_string()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        };
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What a command did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    /// The exit code, or `None` if it was killed.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs a command: `run_system` for real, a stand-in in tests.
pub type Runner<'a> = &'a dyn Fn(&Cmd) -> std::io::Result<Ran>;

/// Runs `cmd` with nothing on its stdin and its output captured.
pub fn run_system(cmd: &Cmd) -> std::io::Result<Ran> {
    let mut c = Command::new(&cmd.program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for a in &cmd.args {
            c.raw_arg(a);
        }
    }
    #[cfg(not(windows))]
    c.args(&cmd.args);
    let out = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    Ok(Ran {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

fn tool_missing(tool: &str, what: &str) -> ConvError {
    ConvError::new(
        ErrorCode::BackendMissing,
        format!("conv wifi reads {what} with {tool}, which isn't here"),
    )
}

/// Runs `cmd`, naming the tool when it isn't there.
fn ask(run: Runner, cmd: &Cmd, what: &str) -> Result<Ran> {
    run(cmd).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            tool_missing(&cmd.program, what)
        } else {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("couldn't run {}: {e}", cmd.program),
            )
        }
    })
}

/// The first line of a tool's complaint, for an error message. Tools that
/// read a password never print it on stderr.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("it gave no reason")
        .to_string()
}

fn netsh() -> String {
    std::env::var("SystemRoot")
        .map(|root| format!(r"{root}\System32\netsh.exe"))
        .unwrap_or_else(|_| "netsh".to_string())
}

const NMCLI: &str = "nmcli";
const NM_WIFI: &str = "802-11-wireless";

/// The Mac's Wi-Fi device (`en0`), from the hardware ports networksetup
/// lists.
fn mac_device(run: Runner) -> Result<String> {
    let cmd = Cmd::new(NETWORKSETUP, &["-listallhardwareports"]);
    let ran = ask(run, &cmd, "the Wi-Fi settings")?;
    parse_hardware_ports(&ran.out())
        .ok_or_else(|| ConvError::new(ErrorCode::InvalidInvocation, "this Mac has no Wi-Fi"))
}

/// The device of the `Wi-Fi` (once `AirPort`) port in networksetup's list.
pub fn parse_hardware_ports(out: &str) -> Option<String> {
    let mut port = "";
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("Hardware Port:") {
            port = p.trim();
        } else if let Some(d) = line.strip_prefix("Device:") {
            if matches!(port, "Wi-Fi" | "AirPort") {
                return Some(d.trim().to_string());
            }
        }
    }
    None
}

/// The networks in `networksetup -listpreferredwirelessnetworks`, which
/// prints a header and then one tab-indented name a line.
pub fn parse_preferred(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.strip_prefix('\t'))
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The network in `networksetup -getairportnetwork`, which names it on
/// older macOS and says it isn't on one on newer macOS even when it is.
pub fn parse_airport_network(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.split_once("Network:"))
        .map(|(_, n)| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// The `SSID : name` line of `ipconfig getsummary`, unless macOS hid it
/// (`<redacted>`, without Location access).
pub fn parse_ipconfig_ssid(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(" : ")?;
            (k.trim() == "SSID").then(|| v.trim().to_string())
        })
        .filter(|v| !v.is_empty() && v != "<redacted>")
}

/// The command that reads `name`'s password: the one `--dry-run` shows.
pub fn password_command(platform: Platform, name: &str) -> Result<Cmd> {
    match platform {
        Platform::Mac => Ok(Cmd::new(
            SECURITY,
            &["find-generic-password", "-D", AIRPORT_KIND, "-a", name, "-w"],
        )),
        Platform::Windows => {
            if name.contains('"') {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    "netsh can't look up a network whose name has a double quote in it",
                ));
            }
            Ok(Cmd {
                program: netsh(),
                args: vec![
                    "wlan".into(),
                    "show".into(),
                    "profile".into(),
                    format!("name=\"{name}\""),
                    "key=clear".into(),
                ],
            })
        }
        Platform::Linux => Ok(Cmd::new(
            NMCLI,
            &[
                "--escape",
                "no",
                "--show-secrets",
                "--get-values",
                "802-11-wireless.ssid,802-11-wireless.hidden,802-11-wireless-security.key-mgmt,802-11-wireless-security.psk,802-11-wireless-security.wep-key0",
                "connection",
                "show",
                "id",
                name,
            ],
        )),
    }
}

/// The network this computer is on, or `None` when the system won't say
/// (recent macOS hides it from programs without Location access).
pub fn current(platform: Platform, run: Runner) -> Result<Option<String>> {
    match platform {
        Platform::Mac => {
            let device = mac_device(run)?;
            let ran = ask(
                run,
                &Cmd::new(NETWORKSETUP, &["-getairportnetwork", &device]),
                "the Wi-Fi settings",
            )?;
            if let Some(n) = parse_airport_network(&ran.out()).filter(|_| ran.ok()) {
                return Ok(Some(n));
            }
            let ran = ask(
                run,
                &Cmd::new(IPCONFIG, &["getsummary", &device]),
                "the Wi-Fi settings",
            )?;
            Ok(parse_ipconfig_ssid(&ran.out()))
        }
        Platform::Windows => {
            let ran = ask(
                run,
                &Cmd::new(&netsh(), &["wlan", "show", "interfaces"]),
                "Windows' Wi-Fi settings",
            )?;
            Ok(ran.ok().then(|| parse_netsh_ssid(&ran.out())).flatten())
        }
        Platform::Linux => {
            let ran = ask(
                run,
                &Cmd::new(
                    NMCLI,
                    &[
                        "--escape",
                        "no",
                        "--get-values",
                        "NAME,TYPE",
                        "connection",
                        "show",
                        "--active",
                    ],
                ),
                "NetworkManager's networks",
            )?;
            Ok(ran
                .ok()
                .then(|| parse_nmcli_wifi(&ran.out()).into_iter().next())
                .flatten())
        }
    }
}

/// The networks saved on this computer, by the names `read` takes.
pub fn saved(platform: Platform, run: Runner) -> Result<Vec<String>> {
    match platform {
        Platform::Mac => {
            let device = mac_device(run)?;
            let ran = ask(
                run,
                &Cmd::new(NETWORKSETUP, &["-listpreferredwirelessnetworks", &device]),
                "the Wi-Fi settings",
            )?;
            if !ran.ok() {
                return Err(ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!(
                        "couldn't list the saved networks: {}",
                        first_line(&ran.out())
                    ),
                ));
            }
            Ok(parse_preferred(&ran.out()))
        }
        Platform::Windows => {
            let ran = ask(
                run,
                &Cmd::new(&netsh(), &["wlan", "show", "profiles"]),
                "Windows' Wi-Fi settings",
            )?;
            if !ran.ok() {
                return Err(ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!(
                        "couldn't list the saved networks: {}",
                        first_line(&ran.out())
                    ),
                ));
            }
            Ok(parse_netsh_profiles(&ran.out()))
        }
        Platform::Linux => {
            let ran = ask(
                run,
                &Cmd::new(
                    NMCLI,
                    &[
                        "--escape",
                        "no",
                        "--get-values",
                        "NAME,TYPE",
                        "connection",
                        "show",
                    ],
                ),
                "NetworkManager's networks",
            )?;
            if !ran.ok() {
                return Err(ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!(
                        "couldn't list the saved networks: {}",
                        first_line(&ran.err())
                    ),
                ));
            }
            Ok(parse_nmcli_wifi(&ran.out()))
        }
    }
}

/// Reads `name`'s security and password. On macOS this is when macOS asks
/// the person at the computer to allow it.
pub fn read(platform: Platform, name: &str, run: Runner) -> Result<Network> {
    let cmd = password_command(platform, name)?;
    match platform {
        Platform::Mac => {
            let ran = ask(run, &cmd, "the keychain")?;
            read_mac(name, &ran, || saved(platform, run))
        }
        Platform::Windows => {
            let ran = ask(run, &cmd, "Windows' Wi-Fi settings")?;
            read_netsh(name, &ran)
        }
        Platform::Linux => {
            let ran = ask(run, &cmd, "NetworkManager's networks")?;
            read_nmcli(name, &ran)
        }
    }
}

fn not_saved(name: &str, platform: Platform) -> ConvError {
    ConvError {
        code: ErrorCode::InputNotFound,
        message: format!("{} has no saved network called {name}", platform.computer()),
        backend: None,
        remediation: Some(crate::Remediation {
            managed: None,
            manual: Some("conv wifi --list shows the ones it has".to_string()),
        }),
    }
}

/// What `security find-generic-password -w` gave: the password and a
/// newline, or why not. A network saved without a keychain item is open.
fn read_mac(name: &str, ran: &Ran, saved: impl FnOnce() -> Result<Vec<String>>) -> Result<Network> {
    let network = |security, password, notes| Network {
        name: name.to_string(),
        ssid: name.to_string(),
        security,
        label: None,
        password,
        hidden: false,
        notes,
    };
    if ran.ok() {
        let mut out = ran.stdout.clone();
        if out.last() == Some(&b'\n') {
            out.pop();
        }
        let password = String::from_utf8(out).map_err(|_| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("the password saved for {name} isn't text conv can read"),
            )
        })?;
        return Ok(network(Security::Wpa, Some(password), vec![]));
    }
    let err = ran.err();
    let lower = err.to_ascii_lowercase();
    if lower.contains("could not be found") || ran.code == Some(44) {
        if saved()?.iter().any(|n| n == name) {
            return Ok(network(
                Security::Open,
                None,
                vec![format!(
                    "macOS has no password saved for {name}, so it is taken to be an open network"
                )],
            ));
        }
        return Err(not_saved(name, Platform::Mac));
    }
    if lower.contains("cancel") || ran.code == Some(128) {
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            "the password wasn't shared: the macOS prompt was cancelled",
        ));
    }
    if lower.contains("interaction is not allowed") {
        return Err(ConvError {
            code: ErrorCode::ConversionFailed,
            message:
                "macOS has to ask before it shares a Wi-Fi password, and it can't ask from here"
                    .to_string(),
            backend: None,
            remediation: Some(crate::Remediation {
                managed: None,
                manual: Some(
                    "run conv wifi in a terminal on the Mac itself, logged in and unlocked"
                        .to_string(),
                ),
            }),
        });
    }
    Err(ConvError::new(
        ErrorCode::ConversionFailed,
        format!(
            "the keychain didn't give the password: {}",
            first_line(&err)
        ),
    ))
}

/// The value of each `label : value` line of netsh's output, in order.
fn netsh_fields(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(" : ")?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The connected network's `SSID` line in `netsh wlan show interfaces`.
pub fn parse_netsh_ssid(out: &str) -> Option<String> {
    netsh_fields(out)
        .into_iter()
        .find(|(k, _)| k == "SSID")
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// The profile names in `netsh wlan show profiles`: every indented
/// `label : name` line, which holds in any language Windows speaks.
pub fn parse_netsh_profiles(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.starts_with(' '))
        .filter_map(|l| l.split_once(" : "))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// What `netsh wlan show profile name="..." key=clear` gave.
fn read_netsh(name: &str, ran: &Ran) -> Result<Network> {
    let out = ran.out();
    if !ran.ok() {
        if out.to_ascii_lowercase().contains("not found") {
            return Err(not_saved(name, Platform::Windows));
        }
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("netsh didn't give the password: {}", first_line(&out)),
        ));
    }
    parse_netsh_profile(name, &out)
}

/// Reads netsh's profile listing. Its labels are in English here; Windows
/// in another language words them differently.
pub fn parse_netsh_profile(name: &str, out: &str) -> Result<Network> {
    let fields = netsh_fields(out);
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    let auth = get("Authentication").ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            "couldn't read netsh's answer; conv reads it in English, and this Windows may speak another language",
        )
    })?;
    let cipher = get("Cipher").unwrap_or_default();
    let lower = auth.to_ascii_lowercase();
    let security = if lower.contains("enterprise") || lower.contains("802.1x") {
        Security::Enterprise
    } else if lower.contains("wpa") {
        Security::Wpa
    } else if lower == "shared" || cipher.eq_ignore_ascii_case("wep") {
        Security::Wep
    } else {
        Security::Open
    };
    let ssid = get("SSID name")
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|| name.to_string());
    let password = get("Key Content");
    if password.is_none() && matches!(security, Security::Wpa | Security::Wep) {
        return Err(ConvError {
            code: ErrorCode::ConversionFailed,
            message: format!("Windows didn't show the password saved for {name}"),
            backend: None,
            remediation: Some(crate::Remediation {
                managed: None,
                manual: Some("run the terminal as administrator and try again".to_string()),
            }),
        });
    }
    let hidden = get("Network broadcast")
        .is_some_and(|v| v.to_ascii_lowercase().contains("not broadcasting"));
    Ok(Network {
        name: name.to_string(),
        ssid,
        security,
        label: Some(auth),
        password,
        hidden,
        notes: vec![],
    })
}

/// The Wi-Fi connections in `nmcli --escape no -g NAME,TYPE connection
/// show`: a name, a colon and a type a line. The type has no colon, so
/// the name is everything before the last one.
pub fn parse_nmcli_wifi(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.rsplit_once(':'))
        .filter(|(_, t)| *t == NM_WIFI)
        .map(|(n, _)| n.to_string())
        .collect()
}

/// What nmcli gave for the five fields `password_command` asks for.
fn read_nmcli(name: &str, ran: &Ran) -> Result<Network> {
    if !ran.ok() {
        let err = ran.err();
        if err.to_ascii_lowercase().contains("no such connection") {
            return Err(not_saved(name, Platform::Linux));
        }
        return Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("nmcli didn't give the password: {}", first_line(&err)),
        ));
    }
    parse_nmcli_secrets(name, &ran.out())
}

/// nmcli's `--get-values` answer: one value a line, in the order asked
/// for, blank where a field has no value.
pub fn parse_nmcli_secrets(name: &str, out: &str) -> Result<Network> {
    let mut lines = out.lines().map(str::to_string);
    let mut next = || lines.next().unwrap_or_default();
    let (ssid, hidden, key_mgmt, psk, wep) = (next(), next(), next(), next(), next());
    let (security, label, password) = match key_mgmt.as_str() {
        "" | "owe" => (Security::Open, None, None),
        "none" => (Security::Wep, Some("WEP"), Some(wep)),
        "wpa-psk" => (Security::Wpa, Some("WPA/WPA2 Personal"), Some(psk)),
        "sae" => (Security::Wpa, Some("WPA3 Personal"), Some(psk)),
        _ => (Security::Enterprise, Some("WPA Enterprise"), None),
    };
    let password = password.filter(|p| !p.is_empty());
    if password.is_none() && matches!(security, Security::Wpa | Security::Wep) {
        return Err(ConvError {
            code: ErrorCode::ConversionFailed,
            message: format!("NetworkManager didn't share the password saved for {name}"),
            backend: None,
            remediation: Some(crate::Remediation {
                managed: None,
                manual: Some(
                    "it may be saved for all users; try sudo conv wifi, or read it in the network settings"
                        .to_string(),
                ),
            }),
        });
    }
    Ok(Network {
        name: name.to_string(),
        ssid: if ssid.is_empty() {
            name.to_string()
        } else {
            ssid
        },
        security,
        label: label.map(str::to_string),
        password,
        hidden: hidden == "yes",
        notes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn net(security: Security, ssid: &str, password: Option<&str>) -> Network {
        Network {
            name: ssid.to_string(),
            ssid: ssid.to_string(),
            security,
            label: None,
            password: password.map(str::to_string),
            hidden: false,
            notes: vec![],
        }
    }

    #[test]
    fn the_wifi_special_characters_are_escaped() {
        assert_eq!(escape(r#"a\b;c,d"e:f"#), r#"a\\b\;c\,d\"e\:f"#);
        assert_eq!(escape("Café 5G"), "Café 5G");
        assert_eq!(escape(""), "");
    }

    #[test]
    fn join_text_for_each_kind_of_network() {
        assert_eq!(
            net(Security::Wpa, "Home", Some("hunter22"))
                .join_text()
                .unwrap(),
            "WIFI:T:WPA;S:Home;P:hunter22;;"
        );
        assert_eq!(
            net(Security::Wpa, "My;Net", Some(r#"p:a,s"s\"#))
                .join_text()
                .unwrap(),
            r#"WIFI:T:WPA;S:My\;Net;P:p\:a\,s\"s\\;;"#
        );
        assert_eq!(
            net(Security::Open, "Guest", None).join_text().unwrap(),
            "WIFI:T:nopass;S:Guest;;"
        );
        assert_eq!(
            net(Security::Wep, "Old", Some("abcde"))
                .join_text()
                .unwrap(),
            "WIFI:T:WEP;S:Old;P:abcde;;"
        );
        let hidden = Network {
            hidden: true,
            ..net(Security::Wpa, "Attic", Some("pw"))
        };
        assert_eq!(
            hidden.join_text().unwrap(),
            "WIFI:T:WPA;S:Attic;P:pw;H:true;;"
        );
        let e = net(Security::Enterprise, "Work", None)
            .join_text()
            .unwrap_err();
        assert!(e.message.contains("their own account"), "{}", e.message);
        assert!(net(Security::Wpa, "Home", None).join_text().is_err());
    }

    #[test]
    fn a_join_code_with_every_special_character_reads_back() {
        let n = Network {
            hidden: true,
            ..net(
                Security::Wpa,
                r#"Caf\é; "5G", 2:1"#,
                Some(r#"p\a;s,s:w"o"rd"#),
            )
        };
        // conv qr reads every code back before writing it, and refuses one
        // that doesn't give back exactly this text.
        let text = n.join_text().unwrap();
        crate::qr::make(&text, None, 512).unwrap();
    }

    #[test]
    fn debug_never_shows_the_password() {
        let n = net(Security::Wpa, "Home", Some("hunter22"));
        let shown = format!("{n:?}");
        assert!(!shown.contains("hunter22"), "{shown}");
        assert!(shown.contains("<hidden>"), "{shown}");
    }

    #[test]
    fn the_mac_password_command_asks_the_keychain_for_the_airport_item() {
        let cmd = password_command(Platform::Mac, "Home Net").unwrap();
        assert_eq!(cmd.program, "/usr/bin/security");
        assert_eq!(
            cmd.args,
            [
                "find-generic-password",
                "-D",
                "AirPort network password",
                "-a",
                "Home Net",
                "-w"
            ]
        );
        if !cfg!(windows) {
            assert_eq!(
                cmd.display(),
                "/usr/bin/security find-generic-password -D 'AirPort network password' -a 'Home Net' -w"
            );
        }
    }

    #[test]
    fn the_windows_command_quotes_the_name_for_netsh() {
        let cmd = password_command(Platform::Windows, "My Home").unwrap();
        assert!(cmd.program.ends_with("netsh.exe") || cmd.program == "netsh");
        assert_eq!(
            cmd.args,
            ["wlan", "show", "profile", "name=\"My Home\"", "key=clear"]
        );
        assert!(password_command(Platform::Windows, "a\"b").is_err());
    }

    #[test]
    fn the_linux_command_asks_nmcli_for_the_secrets_unescaped() {
        let cmd = password_command(Platform::Linux, "Home").unwrap();
        assert_eq!(cmd.program, "nmcli");
        assert_eq!(
            cmd.args[..4],
            ["--escape", "no", "--show-secrets", "--get-values"]
        );
        assert_eq!(cmd.args[5..], ["connection", "show", "id", "Home"]);
    }

    const PORTS: &str = "\nHardware Port: Ethernet Adapter (en3)\nDevice: en3\nEthernet Address: 2a:4e:db:ec:d2:56\n\nHardware Port: Wi-Fi\nDevice: en0\nEthernet Address: a4:cf:99:56:3b:06\n\nHardware Port: Thunderbolt 1\nDevice: en1\n";

    #[test]
    fn mac_outputs_parse() {
        assert_eq!(parse_hardware_ports(PORTS).as_deref(), Some("en0"));
        assert_eq!(
            parse_hardware_ports("Hardware Port: Ethernet\nDevice: en0\n"),
            None
        );
        assert_eq!(
            parse_preferred("Preferred networks on en0:\n\tHome\n\tCafé Guest\n\t  spaced\n"),
            ["Home", "Café Guest", "  spaced"]
        );
        assert_eq!(
            parse_airport_network("Current Wi-Fi Network: Home\n").as_deref(),
            Some("Home")
        );
        assert_eq!(
            parse_airport_network("You are not associated with an AirPort network.\n"),
            None
        );
        let summary =
            "<dictionary> {\n  BSSID : <redacted>\n  InterfaceType : WiFi\n  SSID : Home\n}\n";
        assert_eq!(parse_ipconfig_ssid(summary).as_deref(), Some("Home"));
        let hidden = summary.replace("SSID : Home", "SSID : <redacted>");
        assert_eq!(parse_ipconfig_ssid(&hidden), None);
    }

    /// A runner that answers each program with what `answer` gives and
    /// records what was run.
    fn fake<'a>(
        log: &'a RefCell<Vec<String>>,
        answer: impl Fn(&Cmd) -> Ran + 'a,
    ) -> impl Fn(&Cmd) -> std::io::Result<Ran> + 'a {
        move |cmd| {
            log.borrow_mut().push(cmd.display());
            Ok(answer(cmd))
        }
    }

    fn said(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A Mac on recent macOS: connected, but the name is hidden.
    fn redacted_mac(cmd: &Cmd) -> Ran {
        match cmd.args[0].as_str() {
            "-listallhardwareports" => said(0, PORTS, ""),
            "-getairportnetwork" => said(0, "You are not associated with an AirPort network.\n", ""),
            "getsummary" => said(0, "<dictionary> {\n  SSID : <redacted>\n}\n", ""),
            "-listpreferredwirelessnetworks" => {
                said(0, "Preferred networks on en0:\n\tHome\n\tGuest\n", "")
            }
            "find-generic-password" if cmd.args[4] == "Home" => said(0, "hunter22\n", ""),
            "find-generic-password" => said(
                44,
                "",
                "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n",
            ),
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn a_hidden_mac_network_name_reads_as_unknown() {
        let log = RefCell::new(vec![]);
        let run = fake(&log, redacted_mac);
        assert_eq!(current(Platform::Mac, &run).unwrap(), None);
        assert_eq!(saved(Platform::Mac, &run).unwrap(), ["Home", "Guest"]);
        let log = log.borrow();
        assert!(
            log.iter().any(|c| c.contains("-getairportnetwork en0")),
            "{log:?}"
        );
        assert!(
            log.iter().any(|c| c.contains("ipconfig getsummary en0")),
            "{log:?}"
        );
        assert!(
            !log.iter().any(|c| c.contains("security")),
            "nothing asked the keychain"
        );
    }

    #[test]
    fn an_older_mac_names_its_network() {
        let log = RefCell::new(vec![]);
        let run = fake(&log, |cmd| match cmd.args[0].as_str() {
            "-listallhardwareports" => said(0, PORTS, ""),
            "-getairportnetwork" => said(0, "Current Wi-Fi Network: Home\n", ""),
            other => panic!("unexpected {other}"),
        });
        assert_eq!(
            current(Platform::Mac, &run).unwrap().as_deref(),
            Some("Home")
        );
    }

    #[test]
    fn a_mac_without_wifi_says_so() {
        let log = RefCell::new(vec![]);
        let run = fake(&log, |_| {
            said(0, "Hardware Port: Ethernet\nDevice: en0\n", "")
        });
        let e = current(Platform::Mac, &run).unwrap_err();
        assert!(e.message.contains("no Wi-Fi"), "{}", e.message);
    }

    #[test]
    fn the_keychain_password_is_read_and_a_missing_one_is_explained() {
        let log = RefCell::new(vec![]);
        let run = fake(&log, redacted_mac);
        let home = read(Platform::Mac, "Home", &run).unwrap();
        assert_eq!(home.password.as_deref(), Some("hunter22"));
        assert_eq!(home.security, Security::Wpa);
        // Saved, with no keychain item: an open network.
        let guest = read(Platform::Mac, "Guest", &run).unwrap();
        assert_eq!(guest.security, Security::Open);
        assert_eq!(guest.password, None);
        assert_eq!(guest.notes.len(), 1);
        // Not saved at all.
        let e = read(Platform::Mac, "Nowhere", &run).unwrap_err();
        assert_eq!(e.code, ErrorCode::InputNotFound);
        assert!(e.message.contains("Nowhere"), "{}", e.message);
    }

    #[test]
    fn a_cancelled_or_impossible_keychain_prompt_is_explained() {
        let cancelled = said(
            128,
            "",
            "security: SecKeychainSearchCopyNext: User canceled the operation.\n",
        );
        let e = read_mac("Home", &cancelled, || Ok(vec![])).unwrap_err();
        assert!(e.message.contains("cancelled"), "{}", e.message);
        let headless = said(36, "", "security: User interaction is not allowed.\n");
        let e = read_mac("Home", &headless, || Ok(vec![])).unwrap_err();
        assert!(e.message.contains("can't ask from here"), "{}", e.message);
        // A password's own trailing spaces are kept; only the newline goes.
        let spaced = said(0, "pass word \n", "");
        let n = read_mac("Home", &spaced, || Ok(vec![])).unwrap();
        assert_eq!(n.password.as_deref(), Some("pass word "));
    }

    const NETSH_PROFILE: &str = "\r\nProfile Home on interface Wi-Fi:\r\n=======================================================================\r\n\r\nApplied: All User Profile\r\n\r\nProfile information\r\n-------------------\r\n    Version                : 1\r\n    Type                   : Wireless LAN\r\n    Name                   : Home\r\n    Control options        :\r\n        Connection mode    : Connect automatically\r\n        Network broadcast  : Connect only if this network is broadcasting\r\n        AutoSwitch         : Do not switch to other networks\r\n\r\nConnectivity settings\r\n---------------------\r\n    Number of SSIDs        : 1\r\n    SSID name              : \"Home 5G\"\r\n    Network type           : Infrastructure\r\n\r\nSecurity settings\r\n-----------------\r\n    Authentication         : WPA2-Personal\r\n    Cipher                 : CCMP\r\n    Authentication         : WPA2-Personal\r\n    Cipher                 : GCMP\r\n    Security key           : Present\r\n    Key Content            : hunter22\r\n";

    #[test]
    fn netsh_outputs_parse() {
        let n = parse_netsh_profile("Home", NETSH_PROFILE).unwrap();
        assert_eq!(n.ssid, "Home 5G");
        assert_eq!(n.security, Security::Wpa);
        assert_eq!(n.label.as_deref(), Some("WPA2-Personal"));
        assert_eq!(n.password.as_deref(), Some("hunter22"));
        assert!(!n.hidden);

        let hidden = NETSH_PROFILE.replace(
            "Connect only if this network is broadcasting",
            "Connect even if the network is not broadcasting",
        );
        assert!(parse_netsh_profile("Home", &hidden).unwrap().hidden);

        let no_key = NETSH_PROFILE.replace("    Key Content            : hunter22\r\n", "");
        let e = parse_netsh_profile("Home", &no_key).unwrap_err();
        assert!(e
            .remediation
            .unwrap()
            .manual
            .unwrap()
            .contains("administrator"));

        let open = NETSH_PROFILE
            .replace("WPA2-Personal", "Open")
            .replace("CCMP", "None")
            .replace("GCMP", "None")
            .replace("    Key Content            : hunter22\r\n", "");
        assert_eq!(
            parse_netsh_profile("Cafe", &open).unwrap().security,
            Security::Open
        );

        let work = NETSH_PROFILE.replace("WPA2-Personal", "WPA2-Enterprise");
        assert_eq!(
            parse_netsh_profile("Work", &work).unwrap().security,
            Security::Enterprise
        );

        let german = "    Authentifizierung      : WPA2-Personal\r\n";
        assert!(parse_netsh_profile("Home", german)
            .unwrap_err()
            .message
            .contains("English"));

        let profiles = "\r\nProfiles on interface Wi-Fi:\r\n\r\nGroup policy profiles (read only)\r\n---------------------------------\r\n    <None>\r\n\r\nUser profiles\r\n-------------\r\n    All User Profile     : Home\r\n    All User Profile     : Cafe : Guest\r\n";
        assert_eq!(parse_netsh_profiles(profiles), ["Home", "Cafe : Guest"]);

        let interfaces = "\r\n    Name                   : Wi-Fi\r\n    State                  : connected\r\n    SSID                   : Home\r\n    BSSID                  : aa:bb:cc:dd:ee:ff\r\n";
        assert_eq!(parse_netsh_ssid(interfaces).as_deref(), Some("Home"));
        assert_eq!(parse_netsh_ssid("    State : disconnected\r\n"), None);
    }

    #[test]
    fn a_missing_netsh_profile_is_not_saved() {
        let ran = said(1, "Profile \"Nowhere\" is not found on the system.\r\n", "");
        assert_eq!(
            read_netsh("Nowhere", &ran).unwrap_err().code,
            ErrorCode::InputNotFound
        );
    }

    #[test]
    fn nmcli_outputs_parse() {
        assert_eq!(
            parse_nmcli_wifi("Wired connection 1:802-3-ethernet\nHome:802-11-wireless\nCafe: Guest:802-11-wireless\nlo:loopback\n"),
            ["Home", "Cafe: Guest"]
        );
        let n = parse_nmcli_secrets("Home", "Home 5G\nno\nwpa-psk\nhunter22\n\n").unwrap();
        assert_eq!((n.ssid.as_str(), n.security), ("Home 5G", Security::Wpa));
        assert_eq!(n.password.as_deref(), Some("hunter22"));
        let n = parse_nmcli_secrets("Attic", "Attic\nyes\nsae\npw\n\n").unwrap();
        assert!(n.hidden);
        assert_eq!(n.label.as_deref(), Some("WPA3 Personal"));
        let n = parse_nmcli_secrets("Cafe", "Cafe\nno\n\n\n\n").unwrap();
        assert_eq!(n.security, Security::Open);
        let n = parse_nmcli_secrets("Old", "Old\nno\nnone\n\nabcde\n").unwrap();
        assert_eq!(
            (n.security, n.password.as_deref()),
            (Security::Wep, Some("abcde"))
        );
        let n = parse_nmcli_secrets("Work", "Work\nno\nwpa-eap\n\n\n").unwrap();
        assert_eq!(n.security, Security::Enterprise);
        let e = parse_nmcli_secrets("Home", "Home\nno\nwpa-psk\n\n\n").unwrap_err();
        assert!(e.message.contains("didn't share"), "{}", e.message);
        let ran = said(10, "", "Error: Nowhere - no such connection profile.\n");
        assert_eq!(
            read_nmcli("Nowhere", &ran).unwrap_err().code,
            ErrorCode::InputNotFound
        );
    }

    #[test]
    fn a_missing_tool_is_named() {
        let run = |_: &Cmd| -> std::io::Result<Ran> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no"))
        };
        let e = saved(Platform::Linux, &run).unwrap_err();
        assert_eq!(e.code, ErrorCode::BackendMissing);
        assert!(e.message.contains("nmcli"), "{}", e.message);
    }

    #[test]
    fn errors_never_carry_the_password() {
        // A keychain answer that fails to be text, holding bytes that look
        // like a password: the message names the network, not the bytes.
        let ran = Ran {
            code: Some(0),
            stdout: b"secret\xff\n".to_vec(),
            stderr: vec![],
        };
        let e = read_mac("Home", &ran, || Ok(vec![])).unwrap_err();
        assert!(!e.message.contains("secret"), "{}", e.message);
    }
}
