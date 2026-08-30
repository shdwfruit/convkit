//! Part 1: offering to install a missing backend and retry, instead of
//! making the user read a `backend_missing` error, run `conv install
//! <backend>` by hand, then re-run their original command.
//!
//! Everything here lives in the `conv` binary, never in `convkit-core` —
//! the hard constraint the brief calls out ("`convkit-core` must never
//! prompt or print"). `convkit-core` keeps returning the same structured
//! `ConvError` it always has; this module decides whether to ask the user
//! about it, and `commands/convert.rs` decides what to do with the answer.

use std::io::{IsTerminal, Write};

use convkit_core::{manifest, Backend};

use crate::cli::Cli;

/// Whether this process can prompt at all: both stdin (where the answer
/// comes from) and stderr (where the question is printed, matching every
/// other progress line this binary emits — see `commands/install.rs`'s own
/// "downloading ..." line) must be real terminals. `std::io::IsTerminal` is
/// the standard-library detector — correct on a Windows console as well as
/// a Unix pty, unlike a hand-rolled guess — so piped stdin (a script, a CI
/// runner, `conv ... < /dev/null`) is reliably detected and never made to
/// hang waiting for an answer that will never come.
fn is_interactive_session() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The exact text of the yes/no prompt for `backend`. Pulled out of
/// `prompt_yes_no` as a pure function, with no stdin/stderr of its own, so
/// its wording can be unit-tested directly rather than only through a real
/// TTY (which, per `should_install`'s own tests, this suite can't drive
/// deterministically in CI).
///
/// When `backend`'s managed download also provisions another backend (see
/// `manifest::bundled_with` — today: `ffprobe` and `ffmpeg` share one
/// Windows zip), the prompt says so up front: a user asked "install
/// ffprobe?" should know that saying yes also installs ffmpeg, not discover
/// it silently afterward.
fn prompt_message(backend: Backend) -> String {
    let bundled = manifest::bundled_with(backend);
    let also = if bundled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = bundled.iter().map(|b| b.exe_name()).collect();
        format!(" (also installs {})", names.join(", "))
    };
    format!(
        "{} is required for this conversion and isn't installed.\nInstall it now?{also} [y/N] ",
        backend.exe_name()
    )
}

/// Prints the yes/no prompt to stderr and reads one line of stdin. Any read
/// failure (EOF, a stream error) is treated as "no" — the same conservative
/// default an unanswered prompt gets — rather than propagating an error
/// through a path that must never panic or hang.
fn prompt_yes_no(backend: Backend) -> bool {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{}", prompt_message(backend));
    let _ = stderr.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Decides whether to install `backend` and retry, for a conversion that
/// just failed with `backend_missing`. Every one of these is a hard "no" on
/// its own — the whole point is that any single one of them must be enough
/// to guarantee no prompt and no hang:
///
/// - `--no-install`: never prompt, never install, regardless of anything
///   else.
/// - no managed build for this platform (`manifest::has_managed_build`,
///   never `Backend::is_managed()` alone — see that function's own docs for
///   why the distinction matters, e.g. `Soffice` and, on every platform
///   today, `Magick`): nothing this binary could actually install anyway.
/// - `--json` or `--quiet`: non-interactive output modes stay exactly as
///   they were — no prompt, structured error, exit 3.
///
/// Past those gates: `--yes` proceeds without asking (for a script that
/// wants this behaviour without a TTY to answer a prompt); otherwise this
/// prompts only when the session is genuinely interactive, and only an
/// affirmative answer proceeds.
pub fn should_install(cli: &Cli, backend: Backend) -> bool {
    if cli.no_install {
        return false;
    }
    if !manifest::has_managed_build(backend) {
        return false;
    }
    if cli.json || cli.quiet {
        return false;
    }
    if cli.yes {
        return true;
    }
    is_interactive_session() && prompt_yes_no(backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `Cli` with every field defaulted except the ones a test
    /// cares about — mirrors `input::tests::cli_for`'s own reasoning: `Cli`
    /// doesn't derive `Default` (clap's `Parser` derive doesn't add one),
    /// and these tests only ever exercise `should_install`'s pure gating
    /// logic, which never reads `paths`/`to`/`outdir`/the per-backend path
    /// overrides.
    ///
    /// Deliberately never exercises the final `is_interactive_session() &&
    /// prompt_yes_no(...)` fall-through: that branch reads the real
    /// process's stdin, and under `cargo test` stdin's terminal-ness
    /// depends on how the test binary itself was launched — inside a real
    /// interactive console, reaching that branch would call `read_line`
    /// and block waiting for an answer that never comes. Every case below
    /// is chosen to return from one of the earlier, pure gates instead, so
    /// this test module can never hang. The genuinely interactive prompt is
    /// exercised end to end by `tests/cli.rs`'s piped-stdin tests instead —
    /// deliberately proving the *non*-interactive side, which is the one
    /// that must never regress into a hang.
    fn test_cli(yes: bool, no_install: bool, json: bool, quiet: bool) -> Cli {
        Cli {
            paths: vec![],
            to: None,
            dry_run: false,
            json,
            overwrite: false,
            quiet,
            yes,
            no_install,
            outdir: None,
            jobs: None,
            ffmpeg_path: None,
            magick_path: None,
            pandoc_path: None,
            soffice_path: None,
            typst_path: None,
            command: None,
        }
    }

    #[test]
    fn no_install_always_wins_even_with_yes_also_set() {
        let cli = test_cli(true, true, false, false);
        assert!(!should_install(&cli, Backend::Ffmpeg));
    }

    /// LibreOffice must never be offered, regardless of every other flag —
    /// `manifest::has_managed_build`, not `Backend::is_managed()`, is what
    /// gates this, and it's `false` for `Soffice` on every platform.
    #[test]
    fn soffice_is_never_offerable() {
        let cli = test_cli(true, false, false, false);
        assert!(!should_install(&cli, Backend::Soffice));
    }

    /// `Backend::Magick` is `is_managed() == true` in principle, but the
    /// manifest verifies zero platforms for it — must never be offerable
    /// either, on every platform this test runs on.
    #[test]
    fn magick_is_never_offerable_on_any_platform() {
        let cli = test_cli(true, false, false, false);
        assert!(!should_install(&cli, Backend::Magick));
    }

    #[test]
    fn json_mode_never_installs_even_with_yes() {
        let cli = test_cli(true, false, true, false);
        assert!(!should_install(&cli, Backend::Ffmpeg));
    }

    #[test]
    fn quiet_mode_never_installs_even_with_yes() {
        let cli = test_cli(true, false, false, true);
        assert!(!should_install(&cli, Backend::Ffmpeg));
    }

    /// `--yes` proceeds without ever touching stdin — the whole point of
    /// the flag ("for scripts that want the behaviour without a TTY").
    /// Guarded on `manifest::has_managed_build` the same way every other
    /// test here is, since whether ffmpeg has a verified build depends on
    /// the platform this test runs on.
    #[test]
    fn yes_proceeds_without_prompting_when_a_managed_build_exists() {
        if manifest::has_managed_build(Backend::Ffmpeg) {
            let cli = test_cli(true, false, false, false);
            assert!(should_install(&cli, Backend::Ffmpeg));
        }
    }

    #[test]
    fn plain_no_install_flag_refuses_a_managed_backend_too() {
        let cli = test_cli(false, true, false, false);
        assert!(!should_install(&cli, Backend::Ffmpeg));
    }

    /// A backend with no bundled sibling (e.g. typst, or ffmpeg/ffprobe off
    /// Windows) gets the plain prompt, with no parenthetical at all.
    #[test]
    fn prompt_message_has_no_bundle_note_when_nothing_is_bundled() {
        if manifest::bundled_with(Backend::Typst).is_empty() {
            let msg = prompt_message(Backend::Typst);
            assert!(msg.contains("typst is required"), "{msg}");
            assert!(msg.contains("Install it now? [y/N] "), "{msg}");
            assert!(!msg.contains("also installs"), "{msg}");
        }
    }

    /// On Windows x64, asking to install `ffprobe` must make clear that
    /// accepting also installs `ffmpeg` — the exact acceptance-check wording
    /// this task calls out ("the prompt should make clear that accepting
    /// installs ffmpeg's bundle").
    #[test]
    fn prompt_message_names_the_bundled_sibling_on_windows_x64() {
        if (manifest::current_os(), manifest::current_arch()) == ("windows", "x64") {
            let msg = prompt_message(Backend::Ffprobe);
            assert!(msg.contains("ffprobe is required"), "{msg}");
            assert!(msg.contains("also installs ffmpeg"), "{msg}");

            let msg = prompt_message(Backend::Ffmpeg);
            assert!(msg.contains("ffmpeg is required"), "{msg}");
            assert!(msg.contains("also installs ffprobe"), "{msg}");
        }
    }
}
