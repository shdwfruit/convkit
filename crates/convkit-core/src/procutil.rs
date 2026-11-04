use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

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
    #[cfg_attr(not(windows), allow(unused_mut))]
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

/// How often, and how long apart, `spawn` tries again on a busy program:
/// about half a second in all.
const BUSY_TRIES: u32 = 10;
const BUSY_PAUSE: Duration = Duration::from_millis(10);

/// `cmd.spawn()`, tried again for a moment while the program is busy.
///
/// Linux refuses to run a program file that some process still has open
/// for writing (`ETXTBSY`). Closing it is not always enough: a process
/// another thread forks while the file is open keeps it open until that
/// process runs its own program, which takes a moment. So a program written
/// and then run while other threads start processes can fail to start for
/// no reason of its own. conv's tests do exactly that, writing stub
/// backends and probing them in parallel threads, and on CI the version
/// probe of a freshly written stub failed this way, read as an unknown
/// version, and `conv update`'s test saw an outdated backend. `conv install`
/// does it too, running the program it has just unpacked.
pub(crate) fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < BUSY_TRIES => {
                tries += 1;
                std::thread::sleep(BUSY_PAUSE * tries);
            }
            spawned => return spawned,
        }
    }
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
