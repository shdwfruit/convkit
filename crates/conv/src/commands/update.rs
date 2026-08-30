//! `conv update` / `conv update --check`.
//!
//! "Up to date" means "matches the version convkit has pinned and
//! verified" -- never "latest upstream". Every managed backend is
//! installed from a pinned URL with a pinned SHA-256 someone verified by
//! downloading the asset; chasing latest upstream would mean fetching an
//! unverified binary, exactly what `manifest.rs` exists to prevent. The
//! consequence, made explicit in `cli.rs`'s long help for this command:
//! updating `conv` itself is what advances the pins -- a newer convkit
//! ships a newer manifest, and this command then brings the machine's
//! backends in line with it.
//!
//! This deliberately never replaces the running `conv` binary. Downloading
//! and swapping the executable that is currently running is a real,
//! platform-specific security surface, and today there is nothing
//! published to fetch anyway (see `conv_self_report`'s own docs). Instead
//! this detects how `conv` was installed, purely from its own executable
//! path, and prints the command that would upgrade it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use convkit_core::{manifest, Backend, ConvError, ErrorCode, Resolver};
use serde::Serialize;
use serde_json::json;

use crate::cli::Cli;
use crate::commands::install;

const BACKENDS: [Backend; 6] = [
    Backend::Ffmpeg,
    Backend::Ffprobe,
    Backend::Magick,
    Backend::Pandoc,
    Backend::Soffice,
    Backend::Typst,
];

/// One backend's row in the report. `--check` and a real update share this
/// exact shape -- the only difference is which `action` values can appear:
/// `--check` (and `--no-install`, which this command treats identically --
/// see `run`'s own docs) never produces `"updated"`/`"error"`, since
/// neither ever calls the installer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BackendReport {
    backend: Backend,
    /// `manifest::has_managed_build(backend)`, not `Backend::is_managed()`
    /// -- `magick` is `is_managed() == true` in principle but has zero
    /// verified manifest entries on any platform, and must be reported as
    /// unmanaged the same as `soffice`, exactly the distinction
    /// `manifest::has_managed_build`'s own docs describe.
    managed: bool,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_version: Option<String>,
    /// `"current" | "outdated" | "missing" | "updated" | "error" | "unmanaged"`.
    /// A plain `&'static str` rather than its own enum: this is a leaf
    /// value with one consumer (`backend_line`/`--json`), and every value
    /// already appears verbatim in this module's own docs and tests, so a
    /// second, parallel vocabulary (enum variant names) would only be
    /// something to keep in sync with these string literals, not a
    /// correctness gain.
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ConvError>,
}

/// How this build of `conv` was installed, detected from its own running
/// executable's path -- no network call, no external command run. Ordered
/// so cargo's `~/.cargo/bin` is checked first: it's this project's actual
/// primary install path today (no crates.io, Homebrew, or Scoop package
/// exists yet), and the three detectors are otherwise mutually exclusive
/// in practice (a real install never lands in more than one of these
/// locations at once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Cargo,
    Homebrew,
    Scoop,
    Unknown,
}

impl InstallMethod {
    fn label(self) -> &'static str {
        match self {
            InstallMethod::Cargo => "cargo",
            InstallMethod::Homebrew => "homebrew",
            InstallMethod::Scoop => "scoop",
            InstallMethod::Unknown => "unknown",
        }
    }

    /// The command that would upgrade `conv` itself, for this install
    /// method. Never run -- only ever printed, the same standing rule
    /// every other package-manager command in this codebase follows.
    fn update_hint(self) -> String {
        match self {
            // No crate is published on crates.io yet (see the README's own
            // Install section), so `cargo install convkit` is offered as
            // the eventual path, not the only one -- `--path <repo>` is
            // what actually works today, for anyone who built from a
            // checkout the way this binary itself was built.
            InstallMethod::Cargo => {
                "cargo install --path <repo> (or, once published: cargo install convkit)"
                    .to_string()
            }
            InstallMethod::Homebrew => "brew upgrade convkit".to_string(),
            InstallMethod::Scoop => "scoop update conv".to_string(),
            InstallMethod::Unknown => {
                format!("download the latest release from {RELEASES_PAGE}")
            }
        }
    }
}

const RELEASES_PAGE: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases");

/// Whether any two adjacent path components (case-insensitively) equal
/// `a` then `b`, in that order -- e.g. `.cargo` immediately followed by
/// `bin`. Component-wise rather than a raw substring match on the path
/// string, so a coincidental directory name (someone's own project called
/// `my-cargo-bin-tools`) can't false-positive the way `contains(".cargo\
/// bin")` could if a separator were ever spelled differently than expected.
fn has_adjacent_components(path: &Path, a: &str, b: &str) -> bool {
    let comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    comps.windows(2).any(|w| w[0] == a && w[1] == b)
}

/// Whether any single path component equals `name`, case-insensitively.
fn has_component(path: &Path, name: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(name))
}

fn detect_install_method(exe: &Path) -> InstallMethod {
    if has_adjacent_components(exe, ".cargo", "bin") {
        InstallMethod::Cargo
    } else if exe.starts_with("/opt/homebrew")
        || exe.starts_with("/usr/local/Cellar")
        || exe.starts_with("/home/linuxbrew")
    {
        InstallMethod::Homebrew
    } else if has_component(exe, "scoop") {
        InstallMethod::Scoop
    } else {
        InstallMethod::Unknown
    }
}

#[derive(Debug, Clone, Serialize)]
struct ConvSelfReport {
    version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exe_path: Option<PathBuf>,
    install_method: &'static str,
    update_hint: String,
}

/// `conv`'s own status: the version actually running (`CARGO_PKG_VERSION`,
/// the same value `#[command(version)]` in `cli.rs` already prints for
/// `conv --version`), and how to upgrade it. Never fails -- when
/// `std::env::current_exe()` itself errors (rare; a handful of exotic
/// sandboxes), `install_method` degrades to `Unknown` and `exe_path` is
/// omitted, rather than this command failing outright over information
/// that was never more than a courtesy.
fn conv_self_report() -> ConvSelfReport {
    let exe_path = std::env::current_exe().ok();
    let method = exe_path
        .as_deref()
        .map(detect_install_method)
        .unwrap_or(InstallMethod::Unknown);
    ConvSelfReport {
        version: env!("CARGO_PKG_VERSION"),
        exe_path,
        install_method: method.label(),
        update_hint: method.update_hint(),
    }
}

/// This backend's report, without installing or changing anything --
/// exactly what `--check` needs, and the starting point `perform_updates`
/// mutates in place for a real `conv update`.
fn classify(resolver: &Resolver, backend: Backend) -> BackendReport {
    if manifest::has_managed_build(backend) {
        classify_managed(resolver, backend)
    } else {
        classify_unmanaged(resolver, backend)
    }
}

fn classify_managed(resolver: &Resolver, backend: Backend) -> BackendReport {
    // `has_managed_build` being true guarantees `lookup` is `Some`,
    // per its own docs -- `.unwrap_or("")` here is belt-and-braces, not an
    // expected path.
    let pinned = manifest::lookup(backend).map_or(String::new(), |a| a.version.to_string());
    match resolver.resolve(backend) {
        Ok(r) => {
            let current = manifest::version_is_current(&r.version, &pinned);
            BackendReport {
                backend,
                managed: true,
                installed: true,
                version: Some(r.version.clone()),
                pinned_version: Some(pinned),
                action: if current { "current" } else { "outdated" },
                path: Some(r.path.clone()),
                manual_hint: None,
                error: None,
            }
        }
        Err(_) => BackendReport {
            backend,
            managed: true,
            installed: false,
            version: None,
            pinned_version: Some(pinned),
            action: "missing",
            path: None,
            manual_hint: None,
            error: None,
        },
    }
}

fn classify_unmanaged(resolver: &Resolver, backend: Backend) -> BackendReport {
    let manual_hint = Some(convkit_core::manual_hint_for(backend));
    match resolver.resolve(backend) {
        Ok(r) => BackendReport {
            backend,
            managed: false,
            installed: true,
            version: Some(r.version.clone()),
            pinned_version: None,
            action: "unmanaged",
            path: Some(r.path.clone()),
            manual_hint,
            error: None,
        },
        Err(_) => BackendReport {
            backend,
            managed: false,
            installed: false,
            version: None,
            pinned_version: None,
            action: "unmanaged",
            path: None,
            manual_hint,
            error: None,
        },
    }
}

/// Reinstalls every managed backend whose report says `"outdated"` or
/// `"missing"`, in place, turning that entry into `"updated"` on success or
/// `"error"` (carrying the failure) otherwise -- every other report is
/// returned untouched, including every unmanaged one and every already-
/// `"current"` one (a no-op backend is never re-downloaded).
///
/// `install_fn` is `install::install_backend` in production
/// (`convkit_core::install::fetch_and_install` under that, so this reuses
/// the one pinned-URL-plus-SHA-256, atomic-temp-then-rename download path
/// rather than a second one); a test substitutes a stub so this function's
/// own sequencing logic -- the part this task actually adds -- is provable
/// without a real network fetch.
///
/// Respects the ffmpeg/ffprobe pairing without any special-casing of that
/// pair by name: `install_fn` for *either* one returns every binary its
/// shared download actually provisioned (on Windows x64, both), which are
/// recorded in `installed_this_run` as they land. `BACKENDS`' fixed order
/// (`Ffmpeg` before `Ffprobe`) means whichever of the two is reached first
/// triggers the one real fetch; when the loop reaches the second, it's
/// already in `installed_this_run` and is relabelled `"updated"` from that
/// cached path instead of being fetched again.
fn perform_updates(
    mut reports: Vec<BackendReport>,
    mut install_fn: impl FnMut(Backend) -> Result<Vec<(Backend, PathBuf)>, ConvError>,
) -> Vec<BackendReport> {
    let mut installed_this_run: HashMap<Backend, PathBuf> = HashMap::new();

    for report in &mut reports {
        if !report.managed || !matches!(report.action, "outdated" | "missing") {
            continue;
        }

        if let Some(path) = installed_this_run.get(&report.backend) {
            report.action = "updated";
            report.path = Some(path.clone());
            report.installed = true;
            report.version = report.pinned_version.clone();
            continue;
        }

        match install_fn(report.backend) {
            Ok(installed) => {
                for (b, p) in installed {
                    installed_this_run.insert(b, p);
                }
                report.action = "updated";
                report.installed = true;
                report.path = installed_this_run.get(&report.backend).cloned();
                report.version = report.pinned_version.clone();
            }
            Err(e) => {
                report.action = "error";
                report.error = Some(e);
            }
        }
    }

    reports
}

/// One line per backend, for human-mode output. Reads only `action` plus
/// the fields that action implies are populated -- this is the single
/// place both `--check` and a real update funnel through, since (per
/// `BackendReport`'s own docs) the two only ever differ in which `action`
/// values appear, never in how a given one is worded.
fn backend_line(r: &BackendReport) -> String {
    let name = r.backend.exe_name();
    match r.action {
        "current" => format!(
            "{name:<8} up to date   ({})",
            r.version.as_deref().unwrap_or("unknown")
        ),
        "updated" => format!(
            "{name:<8} updated      -> {}  {}",
            r.pinned_version.as_deref().unwrap_or("?"),
            r.path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        "outdated" => format!(
            "{name:<8} outdated     installed {}, pinned {}",
            r.version.as_deref().unwrap_or("unknown"),
            r.pinned_version.as_deref().unwrap_or("?"),
        ),
        "missing" => format!(
            "{name:<8} missing      pinned {} not installed",
            r.pinned_version.as_deref().unwrap_or("?"),
        ),
        "error" => format!(
            "{name:<8} update FAILED   {}",
            r.error.as_ref().map(|e| e.message.as_str()).unwrap_or("")
        ),
        "unmanaged" => {
            let state = if r.installed {
                format!("installed {}", r.version.as_deref().unwrap_or("unknown"))
            } else {
                "not installed".to_string()
            };
            format!(
                "{name:<8} unmanaged    {state} -- convkit can't update this; run: {}",
                r.manual_hint.as_deref().unwrap_or("")
            )
        }
        other => format!("{name:<8} {other}"),
    }
}

fn conv_self_line(r: &ConvSelfReport) -> String {
    let mut s = format!("conv {} -- installed via {}", r.version, r.install_method);
    if let Some(p) = &r.exe_path {
        s.push_str(&format!(" ({})", p.display()));
    }
    s.push('\n');
    s.push_str(&format!("  to update: {}\n", r.update_hint));
    s
}

fn print_human(reports: &[BackendReport], conv_report: &ConvSelfReport) {
    for r in reports {
        println!("{}", backend_line(r));
    }
    println!();
    print!("{}", conv_self_line(conv_report));
}

/// `--quiet`'s job here, per the standing rule "suppresses progress but not
/// errors": the reassuring/status table (`print_human`'s whole job, on a
/// no-op run especially) is exactly the "progress" it's meant to silence,
/// but a genuine install failure (`action == "error"`) is not progress --
/// it's the one thing quiet must never hide. Printed to stderr, matching
/// how a real conversion's own per-job failures are reported (see
/// `render::conversion_failure_human`'s callers) rather than to stdout,
/// where quiet has just suppressed everything else.
fn print_quiet_errors(reports: &[BackendReport]) {
    for r in reports.iter().filter(|r| r.action == "error") {
        eprintln!("{}", backend_line(r));
    }
}

fn ok(reports: &[BackendReport], check_only: bool) -> bool {
    if check_only {
        !reports.iter().any(|r| r.managed && r.action != "current")
    } else {
        !reports.iter().any(|r| r.action == "error")
    }
}

fn print_json(reports: &[BackendReport], conv_report: &ConvSelfReport, check_only: bool) {
    let envelope = json!({
        "ok": ok(reports, check_only),
        "backends": reports,
        "conv": conv_report,
    });
    let text = serde_json::to_string_pretty(&envelope).unwrap();
    // Mirrors `doctor`/`capabilities`/`install`'s own documented split (see
    // the README's Machine-readable output section): the envelope goes to
    // stdout on success, stderr on failure -- never split across both, and
    // never gated on `--quiet`, which (per the README) never affects
    // `--json` at all.
    if envelope["ok"].as_bool().unwrap_or(false) {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
}

/// `--check`'s exit code (also used when `--no-install` forces check-only
/// behaviour -- see `run`'s docs): 0 when every managed backend is already
/// `"current"`, otherwise `ErrorCode::BackendMissing`'s exit code (3) --
/// reused rather than invented, since an outdated or missing managed
/// backend is the same underlying condition `backend_missing` already
/// covers ("the fix is `conv install`/`conv update`"), just discovered
/// proactively instead of by a conversion failing on it. Unmanaged backends
/// never affect this: convkit has no pin to compare them against, so
/// nothing about their state is ever "wrong" in the sense this exit code
/// reports.
fn check_exit_code(reports: &[BackendReport]) -> i32 {
    if ok(reports, true) {
        0
    } else {
        ErrorCode::BackendMissing.exit_code()
    }
}

/// A real update's exit code: 0 when nothing errored (whether or not
/// anything needed reinstalling at all). When something did error, this
/// mirrors `commands/convert.rs`'s own batch rule -- `BatchPartialFailure`
/// (4) for a genuinely mixed result (some backends updated fine, at least
/// one didn't), or the lone failure's own error code when every attempted
/// install failed the same way and there is nothing to call "partial"
/// about.
fn update_exit_code(reports: &[BackendReport]) -> i32 {
    let errors: Vec<&ConvError> = reports.iter().filter_map(|r| r.error.as_ref()).collect();
    if errors.is_empty() {
        return 0;
    }
    let attempted = reports
        .iter()
        .filter(|r| matches!(r.action, "updated" | "error"))
        .count();
    if errors.len() == attempted {
        errors[0].code.exit_code()
    } else {
        ErrorCode::BatchPartialFailure.exit_code()
    }
}

/// `conv update` / `conv update --check`.
///
/// `--no-install` is treated exactly like `--check`: its documented meaning
/// elsewhere in this binary is "never install anything, always report the
/// plain state instead" (see `install_prompt.rs`), and `conv update`'s
/// entire purpose is installing, so honouring that flag here means falling
/// back to report-only. `--yes` has no effect on this command at all: it
/// exists to skip an interactive install prompt, and running `conv update`
/// is itself the explicit consent a prompt would otherwise be asking for --
/// there is nothing here to skip asking about.
pub fn run(cli: &Cli, check: bool) -> i32 {
    let check_only = check || cli.no_install;
    let resolver = cli.resolver();

    let mut reports: Vec<BackendReport> =
        BACKENDS.iter().map(|&b| classify(&resolver, b)).collect();
    if !check_only {
        reports = perform_updates(reports, |b| install::install_backend(cli, b));
    }

    let conv_report = conv_self_report();
    let code = if check_only {
        check_exit_code(&reports)
    } else {
        update_exit_code(&reports)
    };

    if cli.json {
        print_json(&reports, &conv_report, check_only);
    } else if !cli.quiet {
        print_human(&reports, &conv_report);
    } else {
        print_quiet_errors(&reports);
    }

    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- classify: pure report-building, no installs -----------------------

    /// Writes a stub whose `-version`/`--version` banner reports exactly
    /// `version` (as the first whitespace-separated digit-bearing token, the
    /// same shape `extract_version_token` in `resolve.rs` expects), so a
    /// test can pin the probed version deterministically -- mirrors the
    /// stub-script pattern this codebase's own tests already use (see
    /// `resolve.rs`, `commands/convert.rs`) rather than depending on
    /// whatever this host happens to have installed.
    fn stub_with_version(dir: &Path, name: &str, version: &str) -> PathBuf {
        let (file_name, body): (String, String) = if cfg!(windows) {
            (
                format!("{name}.bat"),
                format!("@echo off\r\necho {name} {version}\r\nexit /b 0\r\n"),
            )
        } else {
            (
                name.to_string(),
                format!("#!/bin/sh\necho \"{name} {version}\"\n"),
            )
        };
        let p = dir.join(file_name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// A `Resolver` that consults `Source::Override` only -- see
    /// `Resolver::overrides_only`'s own docs -- so these tests are
    /// deterministic regardless of what happens to be on this host's real
    /// `PATH`/`CONVKIT_*` environment/well-known install locations,
    /// including the project's own documented hostile environment (real
    /// ImageMagick and LibreOffice on `PATH`).
    fn overrides_only_resolver() -> Resolver {
        let mut r = Resolver::new();
        r.overrides_only();
        r
    }

    #[test]
    fn classify_reports_current_when_the_probed_version_matches_the_pin() {
        if let Some(asset) = manifest::lookup(Backend::Typst) {
            let dir = tempfile::tempdir().unwrap();
            let stub = stub_with_version(dir.path(), "typst", asset.version);
            let mut r = overrides_only_resolver();
            r.with_override(Backend::Typst, stub);

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(report.installed);
            assert_eq!(report.action, "current");
            assert_eq!(report.pinned_version.as_deref(), Some(asset.version));
        }
    }

    #[test]
    fn classify_reports_outdated_when_the_probed_version_differs_from_the_pin() {
        if manifest::has_managed_build(Backend::Typst) {
            let dir = tempfile::tempdir().unwrap();
            let stub = stub_with_version(dir.path(), "typst", "0.0.1-not-the-pinned-version");
            let mut r = overrides_only_resolver();
            r.with_override(Backend::Typst, stub);

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(report.installed);
            assert_eq!(report.action, "outdated");
        }
    }

    #[test]
    fn classify_reports_missing_when_a_managed_backend_cannot_be_resolved_at_all() {
        if manifest::has_managed_build(Backend::Typst) {
            let r = overrides_only_resolver(); // no override set: candidates() is empty
            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(!report.installed);
            assert_eq!(report.action, "missing");
            assert!(report.version.is_none());
            assert!(report.pinned_version.is_some());
        }
    }

    /// `magick` is `Backend::is_managed() == true` but has zero verified
    /// manifest entries anywhere -- must classify as unmanaged the same as
    /// `soffice`, on every platform this test runs on.
    #[test]
    fn classify_reports_magick_as_unmanaged_on_every_platform() {
        let r = overrides_only_resolver();
        let report = classify(&r, Backend::Magick);
        assert!(!report.managed);
        assert_eq!(report.action, "unmanaged");
        assert!(report.pinned_version.is_none());
        assert!(report.manual_hint.is_some());
    }

    #[test]
    fn classify_reports_soffice_as_unmanaged_and_not_installed_when_unresolvable() {
        let r = overrides_only_resolver();
        let report = classify(&r, Backend::Soffice);
        assert!(!report.managed);
        assert!(!report.installed);
        assert_eq!(report.action, "unmanaged");
        assert!(report.manual_hint.is_some());
    }

    #[test]
    fn classify_reports_an_unmanaged_backend_as_installed_when_it_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let stub = stub_with_version(dir.path(), "soffice", "7.6.4.1");
        let mut r = overrides_only_resolver();
        r.with_override(Backend::Soffice, stub);

        let report = classify(&r, Backend::Soffice);
        assert!(!report.managed);
        assert!(report.installed);
        assert_eq!(report.action, "unmanaged");
        assert_eq!(report.version.as_deref(), Some("7.6.4.1"));
    }

    // --- perform_updates: the shared-download / already-current logic -----

    fn missing_report(backend: Backend, pinned: &str) -> BackendReport {
        BackendReport {
            backend,
            managed: true,
            installed: false,
            version: None,
            pinned_version: Some(pinned.to_string()),
            action: "missing",
            path: None,
            manual_hint: None,
            error: None,
        }
    }

    fn current_report(backend: Backend, version: &str) -> BackendReport {
        BackendReport {
            backend,
            managed: true,
            installed: true,
            version: Some(version.to_string()),
            pinned_version: Some(version.to_string()),
            action: "current",
            path: Some(PathBuf::from(format!("/managed/{}", backend.exe_name()))),
            manual_hint: None,
            error: None,
        }
    }

    /// The mechanism this task exists to add: ffmpeg and ffprobe (or any
    /// two backends sharing one manifest entry) both starting out missing
    /// must trigger exactly one call into the installer, not two -- the
    /// second is satisfied from the first call's own returned bundle.
    #[test]
    fn perform_updates_calls_the_installer_once_for_a_bundled_pair() {
        let reports = vec![
            missing_report(Backend::Ffmpeg, "9.0.1"),
            missing_report(Backend::Ffprobe, "9.0.1"),
        ];
        let calls = std::cell::RefCell::new(Vec::new());
        let updated = perform_updates(reports, |b| {
            calls.borrow_mut().push(b);
            Ok(vec![
                (Backend::Ffmpeg, PathBuf::from("/managed/ffmpeg")),
                (Backend::Ffprobe, PathBuf::from("/managed/ffprobe")),
            ])
        });

        assert_eq!(
            calls.into_inner(),
            vec![Backend::Ffmpeg],
            "must fetch once for the shared download, not once per bundled backend"
        );
        assert!(updated.iter().all(|r| r.action == "updated"));
        assert_eq!(updated[0].path, Some(PathBuf::from("/managed/ffmpeg")));
        assert_eq!(updated[1].path, Some(PathBuf::from("/managed/ffprobe")));
    }

    #[test]
    fn perform_updates_leaves_an_already_current_backend_untouched() {
        let reports = vec![current_report(Backend::Typst, "0.15.1")];
        let mut calls = 0;
        let updated = perform_updates(reports, |_| {
            calls += 1;
            Ok(vec![])
        });
        assert_eq!(calls, 0, "an already-current backend must never be fetched");
        assert_eq!(updated[0].action, "current");
    }

    #[test]
    fn perform_updates_reinstalls_an_outdated_backend() {
        let mut report = missing_report(Backend::Pandoc, "3.11");
        report.action = "outdated";
        report.installed = true;
        report.version = Some("3.10".to_string());

        let updated = perform_updates(vec![report], |_| {
            Ok(vec![(Backend::Pandoc, PathBuf::from("/managed/pandoc"))])
        });
        assert_eq!(updated[0].action, "updated");
        assert_eq!(updated[0].version.as_deref(), Some("3.11"));
        assert_eq!(updated[0].path, Some(PathBuf::from("/managed/pandoc")));
    }

    /// A failed install must be recorded on that backend's own report,
    /// without aborting or otherwise affecting any other backend's update.
    #[test]
    fn perform_updates_records_a_failure_without_stopping_other_backends() {
        let reports = vec![
            missing_report(Backend::Pandoc, "3.11"),
            missing_report(Backend::Typst, "0.15.1"),
        ];
        let updated = perform_updates(reports, |b| {
            if b == Backend::Pandoc {
                Err(ConvError::new(
                    ErrorCode::ConversionFailed,
                    "checksum mismatch",
                ))
            } else {
                Ok(vec![(b, PathBuf::from("/managed/typst"))])
            }
        });
        assert_eq!(updated[0].action, "error");
        assert!(updated[0].error.is_some());
        assert_eq!(updated[1].action, "updated");
    }

    #[test]
    fn perform_updates_never_touches_an_unmanaged_backends_report() {
        let report = BackendReport {
            backend: Backend::Soffice,
            managed: false,
            installed: false,
            version: None,
            pinned_version: None,
            action: "unmanaged",
            path: None,
            manual_hint: Some("winget install TheDocumentFoundation.LibreOffice".to_string()),
            error: None,
        };
        let mut calls = 0;
        let updated = perform_updates(vec![report.clone()], |_| {
            calls += 1;
            Ok(vec![])
        });
        assert_eq!(calls, 0);
        assert_eq!(updated[0], report);
    }

    // --- exit codes ----------------------------------------------------------

    #[test]
    fn check_exit_code_is_zero_when_every_managed_backend_is_current() {
        let reports = vec![
            current_report(Backend::Typst, "0.15.1"),
            BackendReport {
                backend: Backend::Soffice,
                managed: false,
                installed: false,
                version: None,
                pinned_version: None,
                action: "unmanaged",
                path: None,
                manual_hint: Some("...".to_string()),
                error: None,
            },
        ];
        assert_eq!(check_exit_code(&reports), 0);
    }

    #[test]
    fn check_exit_code_is_nonzero_when_a_managed_backend_is_missing() {
        let reports = vec![missing_report(Backend::Pandoc, "3.11")];
        assert_ne!(check_exit_code(&reports), 0);
        assert_eq!(
            check_exit_code(&reports),
            ErrorCode::BackendMissing.exit_code()
        );
    }

    #[test]
    fn check_exit_code_is_nonzero_when_a_managed_backend_is_outdated() {
        let mut report = missing_report(Backend::Pandoc, "3.11");
        report.action = "outdated";
        assert_ne!(check_exit_code(&[report]), 0);
    }

    #[test]
    fn update_exit_code_is_zero_when_nothing_errored() {
        let mut report = missing_report(Backend::Pandoc, "3.11");
        report.action = "updated";
        assert_eq!(update_exit_code(&[report]), 0);
    }

    #[test]
    fn update_exit_code_is_nonzero_when_every_attempted_install_failed() {
        let mut report = missing_report(Backend::Pandoc, "3.11");
        report.action = "error";
        report.error = Some(ConvError::new(ErrorCode::ConversionFailed, "boom"));
        assert_eq!(
            update_exit_code(&[report]),
            ErrorCode::ConversionFailed.exit_code()
        );
    }

    #[test]
    fn update_exit_code_is_batch_partial_failure_for_a_mixed_result() {
        let mut failed = missing_report(Backend::Pandoc, "3.11");
        failed.action = "error";
        failed.error = Some(ConvError::new(ErrorCode::ConversionFailed, "boom"));
        let mut succeeded = missing_report(Backend::Typst, "0.15.1");
        succeeded.action = "updated";
        assert_eq!(
            update_exit_code(&[failed, succeeded]),
            ErrorCode::BatchPartialFailure.exit_code()
        );
    }

    // --- conv-self install-method detection -----------------------------

    #[test]
    fn detects_a_cargo_bin_install() {
        let p = if cfg!(windows) {
            PathBuf::from(r"C:\Users\rick\.cargo\bin\conv.exe")
        } else {
            PathBuf::from("/home/rick/.cargo/bin/conv")
        };
        assert_eq!(detect_install_method(&p), InstallMethod::Cargo);
    }

    #[test]
    fn detects_homebrew_via_the_opt_homebrew_prefix() {
        assert_eq!(
            detect_install_method(Path::new("/opt/homebrew/bin/conv")),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_homebrew_via_the_intel_cellar_prefix() {
        assert_eq!(
            detect_install_method(Path::new("/usr/local/Cellar/convkit/0.1.0/bin/conv")),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_homebrew_via_the_linuxbrew_prefix() {
        assert_eq!(
            detect_install_method(Path::new("/home/linuxbrew/.linuxbrew/bin/conv")),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_a_scoop_install() {
        let p = PathBuf::from(r"C:\Users\rick\scoop\shims\conv.exe");
        assert_eq!(detect_install_method(&p), InstallMethod::Scoop);
    }

    #[test]
    fn falls_back_to_unknown_for_an_unrecognised_location() {
        assert_eq!(
            detect_install_method(Path::new("/usr/local/bin/conv")),
            InstallMethod::Unknown
        );
    }

    #[test]
    fn conv_self_report_always_reports_the_running_cargo_pkg_version() {
        let r = conv_self_report();
        assert_eq!(r.version, env!("CARGO_PKG_VERSION"));
        assert!(!r.update_hint.is_empty());
    }
}
