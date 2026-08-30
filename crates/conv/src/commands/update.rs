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
    /// `manifest::has_managed_build`'s own docs describe. Also true for
    /// `"external"` (review finding F41): a copy convkit could manage but
    /// currently isn't -- `managed` describes whether convkit *could*
    /// manage this backend, never which specific copy this report
    /// describes.
    managed: bool,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_version: Option<String>,
    /// `"current" | "outdated" | "not_installed" | "external" | "updated" |
    /// "error" | "unmanaged"`. A plain `&'static str` rather than its own
    /// enum: this is a leaf value with one consumer (`backend_line`/
    /// `--json`), and every value already appears verbatim in this
    /// module's own docs and tests, so a second, parallel vocabulary (enum
    /// variant names) would only be something to keep in sync with these
    /// string literals, not a correctness gain.
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ConvError>,
}

/// How this build of `conv` was installed, detected from its own running
/// executable's path -- no network call, no external command run -- plus,
/// for `Dist`, the presence of an install receipt on disk. `Dist` is
/// checked first (see `detect_install_method_with_receipt_dir`): it's this
/// project's actual documented primary install path (the README's curl/irm
/// one-liners), and cargo-dist's shell/PowerShell installers default to
/// installing into the exact same `~/.cargo/bin` a real `cargo install`
/// uses, so path shape alone can't tell the two apart (review finding
/// F225). The remaining detectors stay ordered the same as before --
/// `Cargo` next, since it's the fallback for anyone who genuinely built
/// from a checkout -- and are otherwise mutually exclusive in practice (a
/// real install never lands in more than one of these locations at once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Dist,
    Cargo,
    Homebrew,
    Scoop,
    Unknown,
}

impl InstallMethod {
    fn label(self) -> &'static str {
        match self {
            InstallMethod::Dist => "dist",
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
            // Re-running the exact one-liner the README hands out re-runs
            // cargo-dist's installer, which fetches and replaces the
            // binary itself -- the only "self-update" mechanism that
            // actually exists for this install method today (review
            // finding F225: the previous `Cargo` advice of `cargo install
            // --path <repo>` assumed a Rust toolchain and a source
            // checkout a dist-installed user is never expected to have).
            InstallMethod::Dist => {
                if cfg!(windows) {
                    WINDOWS_INSTALLER_ONE_LINER.to_string()
                } else {
                    UNIX_INSTALLER_ONE_LINER.to_string()
                }
            }
            // No crate is published on crates.io yet (see the README's own
            // Install section), so `cargo install convkit` is offered as
            // the eventual path, not the only one -- `--path <repo>` is
            // what actually works today, for anyone who built from a
            // checkout the way this binary itself was built.
            InstallMethod::Cargo => {
                "cargo install --path <repo> (or, once published: cargo install convkit)"
                    .to_string()
            }
            // Review finding F225 part 3: no convkit tap or formula is
            // published anywhere, so `brew upgrade convkit` has never been
            // a command that could work -- offering it was actively wrong
            // advice for a `/usr/local/Cellar` install that, per the
            // README, must have arrived some other way (or by hand).
            InstallMethod::Homebrew => format!(
                "no convkit formula or tap is published yet -- grab the latest release from {RELEASES_PAGE}"
            ),
            InstallMethod::Scoop => "scoop update conv".to_string(),
            InstallMethod::Unknown => {
                format!("download the latest release from {RELEASES_PAGE}")
            }
        }
    }
}

const RELEASES_PAGE: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases");

/// The exact one-liners the README's Install section hands out (see its
/// "Linux / macOS" / "Windows" code blocks) -- built from
/// `CARGO_PKG_REPOSITORY` rather than a second hardcoded copy of the repo
/// URL, the same reasoning `RELEASES_PAGE` above already follows.
const UNIX_INSTALLER_ONE_LINER: &str = concat!(
    "curl --proto '=https' --tlsv1.2 -LsSf ",
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/latest/download/convkit-installer.sh | sh"
);
const WINDOWS_INSTALLER_ONE_LINER: &str = concat!(
    "irm ",
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/latest/download/convkit-installer.ps1 | iex"
);

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

/// Whether `dir` contains a cargo-dist install receipt
/// (`convkit-receipt.json`) -- the file cargo-dist's shell/PowerShell
/// installers write recording how this binary was provisioned. Takes the
/// directory explicitly, rather than resolving it from the environment
/// itself, so a test can point this at an isolated temp directory standing
/// in for `$XDG_CONFIG_HOME/convkit`/`%LOCALAPPDATA%\convkit` -- see
/// `receipt_dir`'s own docs for where that comes from in production.
fn has_receipt_in(dir: &Path) -> bool {
    dir.join("convkit-receipt.json").is_file()
}

/// Where cargo-dist's installers write `convkit-receipt.json`:
/// `$XDG_CONFIG_HOME/convkit` (falling back to `~/.config/convkit`) on
/// Unix, `%LOCALAPPDATA%\convkit` on Windows -- matching the receipt
/// locations cargo-dist's shell/PowerShell installer templates use.
/// `None` when neither `XDG_CONFIG_HOME` nor `HOME` (Unix) --
/// `LOCALAPPDATA` (Windows) -- is set, in which case there is nowhere to
/// even look.
fn receipt_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("convkit"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|base| base.join("convkit"))
    }
}

fn detect_install_method(exe: &Path) -> InstallMethod {
    detect_install_method_with_receipt_dir(exe, receipt_dir().as_deref())
}

/// The real detection logic, taking the receipt directory as an explicit
/// argument (production's real call site passes `receipt_dir()`) so a test
/// can supply an isolated temp directory instead -- the same reasoning
/// `Resolver::with_managed_dir` exists for, and the same pattern this
/// module's own `magick_convert_fallback_applies(backend, is_windows)`
/// (in `resolve.rs`) already uses to keep a platform-dependent branch
/// testable on every host regardless of what that host's real environment
/// happens to contain.
fn detect_install_method_with_receipt_dir(exe: &Path, receipt_dir: Option<&Path>) -> InstallMethod {
    // Checked first, before any path-shape heuristic at all (review
    // finding F225 part 1): cargo-dist's shell/PowerShell installers --
    // the README's curl/irm one-liners, this project's actual documented
    // primary install path -- default to installing into `~/.cargo/bin`,
    // the exact same directory a real `cargo install` uses. Without this
    // receipt check first, every dist install was misclassified as
    // `Cargo` and told to `cargo install --path <repo>`, advice that
    // assumes a Rust toolchain and a source checkout a dist-installed user
    // is never expected to have.
    if receipt_dir.is_some_and(has_receipt_in) {
        return InstallMethod::Dist;
    }

    if has_adjacent_components(exe, ".cargo", "bin") {
        return InstallMethod::Cargo;
    }

    // Review finding F225 part 2: canonicalize before the Homebrew prefix
    // checks -- see `canonical_path_starts_with_any`'s own docs for why.
    if canonical_path_starts_with_any(exe, HOMEBREW_PREFIXES) {
        return InstallMethod::Homebrew;
    }

    if has_component(exe, "scoop") {
        return InstallMethod::Scoop;
    }

    InstallMethod::Unknown
}

/// The three real, hardcoded Homebrew install-prefix roots this project
/// recognises: `opt/homebrew` (Apple Silicon), `/usr/local/Cellar` (Intel
/// Mac), `/home/linuxbrew` (Linuxbrew).
const HOMEBREW_PREFIXES: &[&str] = &["/opt/homebrew", "/usr/local/Cellar", "/home/linuxbrew"];

/// Whether `exe`, after resolving symlinks, starts with any of `prefixes`
/// -- falling back to the raw, uncanonicalized path when canonicalization
/// fails (a synthetic path in a test, or a real one deleted out from under
/// a running process), preserving the pre-F225 behaviour for those cases
/// exactly.
///
/// Review finding F225 part 2: an Intel Mac's `/usr/local/bin/conv` is a
/// symlink into `/usr/local/Cellar/convkit/<version>/bin/conv`, and
/// `std::env::current_exe()` (this function's eventual caller, via
/// `detect_install_method`) never resolves symlinks on its own -- without
/// canonicalizing first, every Intel Homebrew install fell through all the
/// way to `Unknown`. Takes `prefixes` as a parameter, rather than the three
/// real Homebrew locations hardcoded inline, so the canonicalization
/// mechanism itself is directly testable with a real symlink into a
/// throwaway tempdir -- not this machine's actual, shared
/// `/usr/local/Cellar`, which a test must never write to. Same pattern
/// `resolve.rs`'s own `magick_convert_fallback_applies(backend,
/// is_windows)` uses to keep a hardcoded, environment-dependent check
/// testable on every host.
fn canonical_path_starts_with_any(exe: &Path, prefixes: &[&str]) -> bool {
    let resolved = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    prefixes.iter().any(|p| resolved.starts_with(p))
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

/// Review findings F41 and F42, both fixed here together because they're
/// the same root cause: this used to classify whatever `resolver.resolve`
/// turned up *first* -- Override, then Env, then Managed, then PATH, then
/// WellKnown -- against the pin, regardless of which of those five sources
/// actually answered.
///
/// F41: a copy resolved from anywhere other than the managed slot is not
/// something convkit installed, and judging it against the pin was actively
/// harmful -- a newer system ffmpeg on `PATH` (Homebrew, say) got labelled
/// "outdated" and then a plain `conv update` downloaded the older pinned
/// build into the managed dir, which outranks `PATH` on every later run
/// (`Source::Managed` beats `Source::Path`; see `Resolver::candidates`'s
/// ordering) -- silently shadowing a perfectly good install with a
/// worse one. With an explicit `--<backend>-path`/`CONVKIT_<BACKEND>` set,
/// that override is what got judged, so `--check` could never turn green
/// no matter what was actually installed. The fix: probe *only*
/// `Resolver::resolve_managed_only`, the exact file `conv install`/`conv
/// update` itself would write to. Anything the general chain finds beyond
/// that is reported as `"external"` purely for information -- never
/// `"outdated"`, never replaced, and (per `ok`'s docs) never counted
/// toward `--check`'s exit code.
///
/// F42: a managed backend that was simply never installed used to be
/// reported as `"missing"`, which failed `--check` (exit 3) and made a
/// plain `conv update` download it -- unasked, on a fresh machine, for
/// every one of the (today) three managed families at once. "Never
/// installed" and "installed but stale" are different states with
/// different correct responses: only the latter is something `conv
/// update` should silently fix. The fix: report a backend absent from the
/// managed dir as `"not_installed"` -- informational, exit 0 (see `ok`),
/// never downloaded by `perform_updates`. `conv install <backend>` remains
/// the explicit, one-backend-at-a-time way to provision it (see
/// `backend_line`'s `"not_installed"` case).
fn classify_managed(resolver: &Resolver, backend: Backend) -> BackendReport {
    // `has_managed_build` being true guarantees `lookup` is `Some`,
    // per its own docs -- `.unwrap_or("")` here is belt-and-braces, not an
    // expected path.
    let pinned = manifest::lookup(backend).map_or(String::new(), |a| a.version.to_string());

    if let Some(r) = resolver.resolve_managed_only(backend) {
        let current = manifest::version_is_current(&r.version, &pinned);
        return BackendReport {
            backend,
            managed: true,
            installed: true,
            version: Some(r.version.clone()),
            pinned_version: Some(pinned),
            action: if current { "current" } else { "outdated" },
            path: Some(r.path.clone()),
            manual_hint: None,
            error: None,
        };
    }

    // Nothing convkit itself put there. Anything the *general* chain finds
    // from here on is, by definition, a copy convkit doesn't manage.
    match resolver.resolve(backend) {
        Ok(r) => BackendReport {
            backend,
            managed: true,
            installed: true,
            version: Some(r.version.clone()),
            pinned_version: Some(pinned),
            action: "external",
            path: Some(r.path.clone()),
            manual_hint: None,
            error: None,
        },
        Err(_) => BackendReport {
            backend,
            managed: true,
            installed: false,
            version: None,
            pinned_version: Some(pinned),
            action: "not_installed",
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

/// Reinstalls every managed backend whose report says `"outdated"` -- an
/// already-managed copy whose version no longer matches the pin -- turning
/// that entry into `"updated"` on success or `"error"` (carrying the
/// failure) otherwise. Every other report is returned untouched:
/// already-`"current"` (a no-op backend is never re-downloaded), every
/// `"unmanaged"` one, every `"external"` one (review finding F41: a copy
/// convkit doesn't manage is never replaced), and every `"not_installed"`
/// one (review finding F42: a backend simply never installed is
/// provisioned by `conv install <backend>`, an explicit, one-backend act --
/// never by `conv update` reinstalling something that was never there).
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
        if !report.managed || report.action != "outdated" {
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
        "not_installed" => format!(
            "{name:<8} not installed  pinned {} available -- provision it with: conv install {name}",
            r.pinned_version.as_deref().unwrap_or("?"),
        ),
        "external" => format!(
            "{name:<8} external     system {} (not managed by convkit)",
            r.version.as_deref().unwrap_or("unknown"),
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

/// `check_only`'s exit-code predicate reads `action` directly rather than
/// `managed`: since `classify_managed` (review findings F41/F42) can now
/// report a *managed-capable* backend as `"external"` or `"not_installed"`
/// with `managed == true`, gating on `managed` alone would fail `--check`
/// on states that were never supposed to affect it -- a system copy
/// convkit doesn't touch, or a backend simply never installed. Only
/// `"outdated"` -- an already-managed copy whose version has drifted from
/// the pin -- is the failure this exit code exists to report.
fn ok(reports: &[BackendReport], check_only: bool) -> bool {
    if check_only {
        !reports.iter().any(|r| r.action == "outdated")
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
/// behaviour -- see `run`'s docs): 0 when no managed backend is
/// `"outdated"`, otherwise `ErrorCode::BackendMissing`'s exit code (3) --
/// reused rather than invented, since an outdated managed backend is the
/// same underlying condition `backend_missing` already covers ("the fix is
/// `conv install`/`conv update`"), just discovered proactively instead of
/// by a conversion failing on it. Nothing else affects this (review
/// findings F41/F42): an unmanaged backend is never something convkit has
/// a pin to compare against; an `"external"` one is a copy convkit doesn't
/// manage, so its version is never judged; and a `"not_installed"` one is
/// simply a backend nobody has provisioned yet, not a broken state --
/// `conv install <backend>` is how a user opts into that, never something
/// `--check` should fail over.
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

    /// Writes `stub_with_version`'s output directly at the exact path
    /// `Source::Managed` (and therefore `Resolver::resolve_managed_only`)
    /// resolves `backend` to inside `dir` -- unlike `Source::Override`
    /// (any filename accepted), the managed slot is a fixed,
    /// platform-specific filename. Unix-only: on Windows the required
    /// filename ends in `.exe`, and `CreateProcess` decides how to run a
    /// file from that literal extension -- a plain-text stub named
    /// `typst.exe` fails to spawn at all rather than running as a script
    /// (see the identical constraint, and why fabricating a real PE binary
    /// here is out of proportion, documented on
    /// `resolve::tests::resolve_managed_only_finds_a_file_written_at_the_
    /// managed_path` in `convkit-core`). The classification logic these
    /// tests actually exist to prove (F41: only the managed slot is ever
    /// judged against the pin) is exercised end-to-end here on Unix, and
    /// at the `resolve_managed_only`-mechanism level on every platform by
    /// that `convkit-core` test.
    #[cfg(unix)]
    fn stub_at_managed_path(dir: &Path, backend: Backend, version: &str) -> PathBuf {
        stub_with_version(dir, backend.exe_name(), version)
    }

    #[cfg(unix)]
    #[test]
    fn classify_reports_current_when_the_managed_dir_copy_matches_the_pin() {
        if let Some(asset) = manifest::lookup(Backend::Typst) {
            let managed_dir = tempfile::tempdir().unwrap();
            stub_at_managed_path(managed_dir.path(), Backend::Typst, asset.version);
            let mut r = Resolver::new();
            r.with_managed_dir(managed_dir.path().to_path_buf());

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(report.installed);
            assert_eq!(report.action, "current");
            assert_eq!(report.pinned_version.as_deref(), Some(asset.version));
        }
    }

    #[cfg(unix)]
    #[test]
    fn classify_reports_outdated_when_the_managed_dir_copy_differs_from_the_pin() {
        if manifest::has_managed_build(Backend::Typst) {
            let managed_dir = tempfile::tempdir().unwrap();
            stub_at_managed_path(
                managed_dir.path(),
                Backend::Typst,
                "0.0.1-not-the-pinned-version",
            );
            let mut r = Resolver::new();
            r.with_managed_dir(managed_dir.path().to_path_buf());

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(report.installed);
            assert_eq!(report.action, "outdated");
        }
    }

    #[test]
    fn classify_reports_not_installed_when_a_managed_backend_is_absent_everywhere() {
        if manifest::has_managed_build(Backend::Typst) {
            let empty_managed_dir = tempfile::tempdir().unwrap();
            let mut r = overrides_only_resolver(); // no override: the general chain is empty too
            r.with_managed_dir(empty_managed_dir.path().to_path_buf());

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(!report.installed);
            assert_eq!(report.action, "not_installed");
            assert!(report.version.is_none());
            assert!(report.pinned_version.is_some());
        }
    }

    /// The headline fix for review finding F41: a copy resolved from
    /// anywhere other than the managed dir -- here, an override standing
    /// in for `--typst-path`/`CONVKIT_TYPST`, or an ordinary `PATH` find --
    /// must be reported as `"external"`, never `"outdated"`, even when its
    /// version plainly does not match the pin. Before this fix, this exact
    /// setup (a real system copy resolved via the general chain, an empty
    /// managed dir) was indistinguishable from a genuinely-managed,
    /// genuinely-stale install.
    #[test]
    fn classify_reports_a_backend_resolved_outside_the_managed_dir_as_external_never_outdated() {
        if manifest::has_managed_build(Backend::Typst) {
            let empty_managed_dir = tempfile::tempdir().unwrap();
            let external_dir = tempfile::tempdir().unwrap();
            let stub =
                stub_with_version(external_dir.path(), "typst", "0.0.1-not-the-pinned-version");
            let mut r = Resolver::new();
            r.with_managed_dir(empty_managed_dir.path().to_path_buf());
            r.with_override(Backend::Typst, stub.clone());

            let report = classify(&r, Backend::Typst);
            assert!(report.managed);
            assert!(report.installed);
            assert_eq!(report.action, "external");
            assert_eq!(
                report.version.as_deref(),
                Some("0.0.1-not-the-pinned-version")
            );
            assert_eq!(report.path.as_deref(), Some(stub.as_path()));
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

    /// A managed backend nobody has ever installed -- review finding F42's
    /// `"not_installed"`, the state a plain `conv update` must never
    /// download for.
    fn not_installed_report(backend: Backend, pinned: &str) -> BackendReport {
        BackendReport {
            backend,
            managed: true,
            installed: false,
            version: None,
            pinned_version: Some(pinned.to_string()),
            action: "not_installed",
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

    /// A backend already present in the managed dir, at a version that no
    /// longer matches the pin -- the one state `perform_updates` actually
    /// reinstalls (review finding F42 narrowed this from "outdated or
    /// missing" down to "outdated" alone).
    fn outdated_report(backend: Backend, installed_version: &str, pinned: &str) -> BackendReport {
        BackendReport {
            backend,
            managed: true,
            installed: true,
            version: Some(installed_version.to_string()),
            pinned_version: Some(pinned.to_string()),
            action: "outdated",
            path: Some(PathBuf::from(format!("/managed/{}", backend.exe_name()))),
            manual_hint: None,
            error: None,
        }
    }

    /// A copy resolved from outside the managed dir -- review finding
    /// F41's `"external"`. `pinned_version` is deliberately a value that
    /// would never match `version` (proving a mismatch alone is not what
    /// triggers a reinstall for this action).
    fn external_report(backend: Backend, version: &str, path: &str) -> BackendReport {
        BackendReport {
            backend,
            managed: true,
            installed: true,
            version: Some(version.to_string()),
            pinned_version: Some("9.9.9-not-what-version-says".to_string()),
            action: "external",
            path: Some(PathBuf::from(path)),
            manual_hint: None,
            error: None,
        }
    }

    /// The mechanism this task exists to add: ffmpeg and ffprobe (or any
    /// two backends sharing one manifest entry) both starting out outdated
    /// must trigger exactly one call into the installer, not two -- the
    /// second is satisfied from the first call's own returned bundle.
    #[test]
    fn perform_updates_calls_the_installer_once_for_a_bundled_pair() {
        let reports = vec![
            outdated_report(Backend::Ffmpeg, "8.0", "9.0.1"),
            outdated_report(Backend::Ffprobe, "8.0", "9.0.1"),
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
        let report = outdated_report(Backend::Pandoc, "3.10", "3.11");

        let updated = perform_updates(vec![report], |_| {
            Ok(vec![(Backend::Pandoc, PathBuf::from("/managed/pandoc"))])
        });
        assert_eq!(updated[0].action, "updated");
        assert_eq!(updated[0].version.as_deref(), Some("3.11"));
        assert_eq!(updated[0].path, Some(PathBuf::from("/managed/pandoc")));
    }

    /// Review finding F42: a managed backend that was simply never
    /// installed must never be downloaded by plain `conv update` -- only
    /// `conv install <backend>` provisions it. Before this fix, this exact
    /// report (`"missing"`, the old name for this state) triggered exactly
    /// the same install call an `"outdated"` one does.
    #[test]
    fn perform_updates_never_downloads_a_backend_that_was_simply_never_installed() {
        let reports = vec![not_installed_report(Backend::Pandoc, "3.11")];
        let mut calls = 0;
        let updated = perform_updates(reports, |_| {
            calls += 1;
            Ok(vec![])
        });
        assert_eq!(
            calls, 0,
            "a never-installed backend must never trigger a download from plain `conv update`"
        );
        assert_eq!(updated[0].action, "not_installed");
    }

    /// A failed install must be recorded on that backend's own report,
    /// without aborting or otherwise affecting any other backend's update.
    #[test]
    fn perform_updates_records_a_failure_without_stopping_other_backends() {
        let reports = vec![
            outdated_report(Backend::Pandoc, "3.10", "3.11"),
            outdated_report(Backend::Typst, "0.15.0", "0.15.1"),
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

    /// Review finding F41's counterpart to the unmanaged test above: a
    /// copy convkit resolved but doesn't manage must never be reinstalled
    /// either, even though `managed` is `true` on this report (see
    /// `BackendReport::managed`'s own docs on why that field alone isn't
    /// enough to gate this).
    #[test]
    fn perform_updates_never_touches_an_external_backends_report() {
        let report = external_report(Backend::Ffmpeg, "9.0", "/usr/local/bin/ffmpeg");
        let mut calls = 0;
        let updated = perform_updates(vec![report.clone()], |_| {
            calls += 1;
            Ok(vec![])
        });
        assert_eq!(
            calls, 0,
            "a copy convkit doesn't manage must never be reinstalled"
        );
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

    /// Review finding F42: a managed backend simply never installed must
    /// never fail `--check` -- it's informational, exit 0. Before this
    /// fix, this exact state (then called `"missing"`) was
    /// indistinguishable from a genuinely broken install and exited 3.
    #[test]
    fn check_exit_code_is_zero_when_a_managed_backend_was_never_installed() {
        let reports = vec![not_installed_report(Backend::Pandoc, "3.11")];
        assert_eq!(check_exit_code(&reports), 0);
    }

    #[test]
    fn check_exit_code_is_nonzero_when_a_managed_backend_is_outdated() {
        let report = outdated_report(Backend::Pandoc, "3.10", "3.11");
        assert_ne!(check_exit_code(&[report]), 0);
        assert_eq!(
            check_exit_code(&[outdated_report(Backend::Pandoc, "3.10", "3.11")]),
            ErrorCode::BackendMissing.exit_code(),
        );
    }

    /// Review finding F41: a copy convkit doesn't manage must never fail
    /// `--check`, no matter how far its version is from the pin --
    /// `external_report`'s own docs explain why its `pinned_version` is
    /// deliberately a mismatch.
    #[test]
    fn check_exit_code_is_zero_when_a_managed_backend_resolves_only_externally() {
        let reports = vec![external_report(
            Backend::Ffmpeg,
            "9.0",
            "/usr/local/bin/ffmpeg",
        )];
        assert_eq!(check_exit_code(&reports), 0);
    }

    #[test]
    fn update_exit_code_is_zero_when_nothing_errored() {
        let mut report = not_installed_report(Backend::Pandoc, "3.11");
        report.action = "updated";
        assert_eq!(update_exit_code(&[report]), 0);
    }

    #[test]
    fn update_exit_code_is_nonzero_when_every_attempted_install_failed() {
        let mut report = not_installed_report(Backend::Pandoc, "3.11");
        report.action = "error";
        report.error = Some(ConvError::new(ErrorCode::ConversionFailed, "boom"));
        assert_eq!(
            update_exit_code(&[report]),
            ErrorCode::ConversionFailed.exit_code()
        );
    }

    #[test]
    fn update_exit_code_is_batch_partial_failure_for_a_mixed_result() {
        let mut failed = not_installed_report(Backend::Pandoc, "3.11");
        failed.action = "error";
        failed.error = Some(ConvError::new(ErrorCode::ConversionFailed, "boom"));
        let mut succeeded = not_installed_report(Backend::Typst, "0.15.1");
        succeeded.action = "updated";
        assert_eq!(
            update_exit_code(&[failed, succeeded]),
            ErrorCode::BatchPartialFailure.exit_code()
        );
    }

    // --- conv-self install-method detection -----------------------------
    //
    // Every test below calls `detect_install_method_with_receipt_dir`
    // directly, passing `None` for the receipt directory unless the test is
    // specifically about receipt detection -- `detect_install_method`
    // itself resolves the *real* `receipt_dir()`, and a contributor machine
    // that has genuinely run the dist installer for real would otherwise
    // make these spuriously see `InstallMethod::Dist` regardless of the
    // path being tested. Same reasoning `Resolver::overrides_only`'s own
    // docs give for why a test must control every seam a real host's state
    // could otherwise leak through.

    #[test]
    fn detects_a_cargo_bin_install() {
        let p = if cfg!(windows) {
            PathBuf::from(r"C:\Users\rick\.cargo\bin\conv.exe")
        } else {
            PathBuf::from("/home/rick/.cargo/bin/conv")
        };
        assert_eq!(
            detect_install_method_with_receipt_dir(&p, None),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn detects_homebrew_via_the_opt_homebrew_prefix() {
        assert_eq!(
            detect_install_method_with_receipt_dir(Path::new("/opt/homebrew/bin/conv"), None),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_homebrew_via_the_intel_cellar_prefix() {
        assert_eq!(
            detect_install_method_with_receipt_dir(
                Path::new("/usr/local/Cellar/convkit/0.1.0/bin/conv"),
                None
            ),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn detects_homebrew_via_the_linuxbrew_prefix() {
        assert_eq!(
            detect_install_method_with_receipt_dir(
                Path::new("/home/linuxbrew/.linuxbrew/bin/conv"),
                None
            ),
            InstallMethod::Homebrew
        );
    }

    /// Review finding F225 part 2: an Intel Mac's `/usr/local/bin/conv` is
    /// a symlink into `/usr/local/Cellar/convkit/<version>/bin/conv`, and
    /// `std::env::current_exe()` never resolves that on its own -- without
    /// canonicalizing first, this exact, real-world Homebrew install was
    /// misclassified as `Unknown`. Exercises the real production helper,
    /// `canonical_path_starts_with_any` (parameterized on the prefix list
    /// for exactly this reason -- see its own docs), with a real symlink
    /// into a throwaway tempdir standing in for the prefix, rather than
    /// this machine's actual, shared `/usr/local/Cellar`, which a test
    /// must never write to. Unix-only: Windows has no symlink-based
    /// package layout this detector needs to see through.
    #[cfg(unix)]
    #[test]
    fn detects_homebrew_through_a_symlink_via_canonicalization() {
        // Canonicalized up front: on macOS, `tempfile::tempdir()` itself
        // can land under a path with its own symlink component (`/var` ->
        // `/private/var`), which is entirely incidental to what this test
        // is actually about -- comparing both sides in already-canonical
        // form isolates the one symlink hop (`symlink` -> `real`) this
        // test exists to prove `canonical_path_starts_with_any` follows,
        // the same way production's three real Homebrew prefixes are
        // already themselves in canonical form.
        let prefix_dir = tempfile::tempdir().unwrap();
        let canonical_prefix_dir = std::fs::canonicalize(prefix_dir.path()).unwrap();
        let real = canonical_prefix_dir.join("conv");
        std::fs::write(&real, b"fake binary").unwrap();

        let link_dir = tempfile::tempdir().unwrap();
        let symlink = link_dir.path().join("conv");
        std::os::unix::fs::symlink(&real, &symlink).unwrap();

        // The symlink's own path doesn't fall under the prefix dir at all
        // -- only its canonicalized target does. This is exactly the shape
        // the production check must see through.
        assert!(
            !symlink.starts_with(&canonical_prefix_dir),
            "the symlink's own path must not already satisfy the prefix -- \
             only its canonicalized target does"
        );
        let prefix = canonical_prefix_dir.to_str().unwrap();
        assert!(canonical_path_starts_with_any(&symlink, &[prefix]));

        // The complement, proving this isn't just a tautology: an
        // uncanonicalized check (equivalent to the pre-F225 behaviour)
        // would miss it.
        assert!(!symlink.to_string_lossy().starts_with(prefix));
    }

    /// Scoop is a Windows-only package manager, so its shim path is
    /// necessarily backslash-separated -- and `Path::components()` only
    /// treats `\` as a separator when the host itself is Windows (on Unix
    /// it's an ordinary filename character, so this literal path collapses
    /// into a single opaque component, `has_component` never sees a
    /// `scoop` component, and detection silently falls through to
    /// `Unknown`). Rather than contort `detect_install_method` to parse
    /// both separators unconditionally -- production Scoop paths only ever
    /// arrive from the real Windows filesystem, which already speaks
    /// backslash -- this test is gated to the one platform where the input
    /// it constructs is representative at all. See
    /// `detects_a_cargo_bin_install` for the alternative this module uses
    /// when a install method genuinely exists cross-platform: branch on
    /// `cfg!(windows)` at runtime to build a platform-appropriate path,
    /// rather than gating the whole test out. That doesn't apply here
    /// because there is no non-Windows Scoop path to branch to.
    #[cfg(windows)]
    #[test]
    fn detects_a_scoop_install() {
        let p = PathBuf::from(r"C:\Users\rick\scoop\shims\conv.exe");
        assert_eq!(
            detect_install_method_with_receipt_dir(&p, None),
            InstallMethod::Scoop
        );
    }

    #[test]
    fn falls_back_to_unknown_for_an_unrecognised_location() {
        assert_eq!(
            detect_install_method_with_receipt_dir(Path::new("/usr/local/bin/conv"), None),
            InstallMethod::Unknown
        );
    }

    /// Review finding F225 part 1: a `convkit-receipt.json` at the receipt
    /// directory means this binary was provisioned by cargo-dist's
    /// shell/PowerShell installer -- the README's curl/irm one-liner, this
    /// project's actual documented primary install path -- which happens
    /// to also land in `~/.cargo/bin` (see the `InstallMethod::Cargo`
    /// docs), so this must win over the `.cargo/bin` path-shape heuristic,
    /// not lose to it. `xdg_config_home` stands in for
    /// `$XDG_CONFIG_HOME`/`%LOCALAPPDATA%` -- see `receipt_dir`'s own docs
    /// for where that comes from in production -- with `convkit/convkit-
    /// receipt.json` written inside it, mirroring the real layout exactly.
    #[test]
    fn a_receipt_directory_containing_the_receipt_file_is_detected_as_a_dist_install() {
        let xdg_config_home = tempfile::tempdir().unwrap();
        let receipt_dir = xdg_config_home.path().join("convkit");
        std::fs::create_dir_all(&receipt_dir).unwrap();
        std::fs::write(receipt_dir.join("convkit-receipt.json"), "{}").unwrap();

        // Even a path that would otherwise be classified `Cargo` must
        // yield `Dist` once a receipt is present -- the whole point of
        // finding F225.
        let exe = PathBuf::from("/home/rick/.cargo/bin/conv");
        assert_eq!(
            detect_install_method_with_receipt_dir(&exe, Some(&receipt_dir)),
            InstallMethod::Dist
        );
    }

    #[test]
    fn no_receipt_file_falls_through_to_the_ordinary_path_based_detection() {
        let receipt_dir = tempfile::tempdir().unwrap(); // exists, but empty
        let exe = PathBuf::from("/home/rick/.cargo/bin/conv");
        assert_eq!(
            detect_install_method_with_receipt_dir(&exe, Some(receipt_dir.path())),
            InstallMethod::Cargo
        );
    }

    /// The other half of finding F225 part 1: the update hint for a
    /// detected dist install must be the exact README one-liner for this
    /// platform, not the cargo-toolchain advice a dist-installed user has
    /// no use for.
    #[test]
    fn dist_update_hint_points_at_the_readme_installer_one_liner() {
        let hint = InstallMethod::Dist.update_hint();
        if cfg!(windows) {
            assert!(hint.contains("convkit-installer.ps1"), "{hint}");
            assert!(hint.starts_with("irm "), "{hint}");
        } else {
            assert!(hint.contains("convkit-installer.sh"), "{hint}");
            assert!(hint.starts_with("curl "), "{hint}");
        }
    }

    /// Review finding F225 part 3: no convkit tap or formula exists, so
    /// `brew upgrade convkit` was never a command that could actually
    /// work.
    #[test]
    fn homebrew_update_hint_no_longer_claims_a_brew_upgrade_command_works() {
        let hint = InstallMethod::Homebrew.update_hint();
        assert!(!hint.contains("brew upgrade"), "{hint}");
        assert!(hint.contains(RELEASES_PAGE), "{hint}");
    }

    #[test]
    fn conv_self_report_always_reports_the_running_cargo_pkg_version() {
        let r = conv_self_report();
        assert_eq!(r.version, env!("CARGO_PKG_VERSION"));
        assert!(!r.update_hint.is_empty());
    }
}
