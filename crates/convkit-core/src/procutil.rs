use std::path::Path;
use std::process::Command;

/// Builds a `Command` for spawning `path` as a backend subprocess, with
/// Windows' console-window suppression applied unconditionally.
///
/// `std::process::Command` does not suppress a console window on its own:
/// spawning a console-subsystem child on Windows (every backend this crate
/// shells out to -- ffmpeg, ffprobe, magick, soffice.com, pandoc, typst --
/// ships as one) gives it its own visible console window unless the parent
/// explicitly passes the `CREATE_NO_WINDOW` creation flag. For most
/// invocations that is an unwanted flash-and-vanish window; for
/// `soffice.com` specifically it is worse -- the console-subsystem
/// executable can end up attached to a console it never actually reads
/// from, observed on this project as a window reading "Press Enter to
/// continue..." that never receives one. `VERSION_PROBE_TIMEOUT` bounds how
/// long that can hang a version probe, but only after several seconds and a
/// visible, distinctly broken-looking window -- a CLI meant to run
/// unattended and scripted should never put a window on screen for this at
/// all.
///
/// Every production spawn site that shells out to a real backend --
/// `exec::run`'s conversion invocation, `resolve::Resolver::
/// probe_first_line` (both the ordinary version probe and the ImageMagick-6
/// `convert` identification probe route through it), and `probe::run`'s
/// ffprobe call -- goes through this one function rather than each calling
/// `Command::new` directly and separately remembering to guard it with its
/// own `#[cfg(windows)]` block. One place to get the flag right, one place
/// to test, rather than the flag silently missing from whichever call site
/// was added or touched last.
///
/// `install.rs`'s `codesign` invocation deliberately does *not* go through
/// this: it's gated `#[cfg(all(target_os = "macos", target_arch =
/// "aarch64"))]` and never compiles on Windows at all, so there is no
/// console window for it to suppress.
pub(crate) fn backend_command(path: impl AsRef<Path>) -> Command {
    let mut cmd = Command::new(path.as_ref());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // https://learn.microsoft.com/windows/win32/procthread/process-creation-flags
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not much to assert cross-platform about a `Command`'s private
    /// creation-flags field (the standard library exposes no getter), so
    /// this only proves the function is at least wired up: it returns a
    /// `Command` targeting the given path, ready for a caller to add args
    /// and spawn -- the same shape every call site already expects from a
    /// bare `Command::new`.
    #[test]
    fn backend_command_targets_the_given_path() {
        let cmd = backend_command("some-backend");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("some-backend"));
    }
}
