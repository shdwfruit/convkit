use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Component, Path};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// `p` as a backend argument. Backends read a relative path whose first
/// part starts with `-` as an option, with `@` as a file of more arguments
/// (qpdf, ImageMagick), and with a `:` in it as a protocol or a format
/// (ffmpeg's `concat:`, ImageMagick's `png:`, pandoc's `http:`), so such a
/// path is given as `./-name`. Every other path comes back as it is,
/// absolute or not, a Windows drive's (`C:\x`, `C:x`) among them.
pub(crate) fn path_arg_os(p: &Path) -> OsString {
    let risky = match p.components().next() {
        Some(Component::Normal(first)) => {
            let first = first.to_string_lossy();
            first.starts_with(['-', '@']) || first.contains(':')
        }
        _ => false,
    };
    let mut arg = OsString::from(if risky { "./" } else { "" });
    arg.push(p);
    arg
}

/// `path_arg_os` as text, for an argv kept as `String`s. `exec::run`
/// refuses a path that is not valid UTF-8, so a plan's paths lose nothing.
pub(crate) fn path_arg(p: &Path) -> String {
    path_arg_os(p).to_string_lossy().into_owned()
}

/// `p[frame]`, ImageMagick's frame selector on `p` as `path_arg_os` gives
/// it. With a selector ImageMagick reads `%d` in the name as a pattern and
/// `%%` as `%`, so a `%` is doubled.
pub(crate) fn magick_frame(p: &Path, frame: impl std::fmt::Display) -> OsString {
    let arg = path_arg_os(p);
    let mut arg = match arg.to_str() {
        Some(s) => OsString::from(s.replace('%', "%%")),
        // shortcut: a non-UTF-8 name keeps its `%` single, upgrade if one with `%d` shows up.
        None => arg,
    };
    arg.push(format!("[{frame}]"));
    arg
}

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

/// Whether qpdf must be given `args` on stdin (`@-`) rather than on its
/// command line. Its Windows build expands wildcards in its arguments, and
/// the `?` of an extended-length path is one: `\\?\C:\docs\report.pdf`
/// reaches qpdf as `\\report.pdf` (seen on the Windows CI job). Arguments
/// qpdf reads from `@-` are taken as they are. A file name on Windows can
/// hold neither `?` nor `*`, so only such a path ever needs it.
pub(crate) fn qpdf_wants_args_on_stdin<S: AsRef<OsStr>>(args: &[S]) -> bool {
    cfg!(windows)
        && args
            .iter()
            .any(|a| a.as_ref().to_string_lossy().contains(['?', '*']))
}

/// Runs qpdf at `qpdf` with `args`, on stdin when its command line would
/// mangle them (`qpdf_wants_args_on_stdin`).
pub(crate) fn qpdf_output(qpdf: &Path, args: &[OsString]) -> std::io::Result<Output> {
    if qpdf_wants_args_on_stdin(args) {
        qpdf_with_args_on_stdin(qpdf, args)
    } else {
        backend_command(qpdf).args(args).output()
    }
}

/// Runs qpdf with `@-` and `args` on stdin, one a line, as qpdf reads an
/// argument file.
pub(crate) fn qpdf_with_args_on_stdin(qpdf: &Path, args: &[OsString]) -> std::io::Result<Output> {
    let mut child = spawn(
        backend_command(qpdf)
            .arg("@-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let mut lines = String::new();
    for a in args {
        lines.push_str(&a.to_string_lossy());
        lines.push('\n');
    }
    if let Some(mut stdin) = child.stdin.take() {
        // A qpdf that exits early closes the pipe; its stderr says why.
        let _ = stdin.write_all(lines.as_bytes());
    }
    child.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_a_backend_would_read_as_something_else_gets_a_dot_slash() {
        let arg = |s: &str| path_arg(Path::new(s));
        assert_eq!(arg("-d.md"), "./-d.md");
        assert_eq!(arg("@list.pdf"), "./@list.pdf");
        assert_eq!(arg("clip-10:30.mp4"), "./clip-10:30.mp4");
        assert_eq!(arg("take:2/out.mp3"), "./take:2/out.mp3");
        assert_eq!(arg("my report.pdf"), "my report.pdf");
        assert_eq!(arg("dir/-x.pdf"), "dir/-x.pdf");
        assert_eq!(arg("./-x.pdf"), "./-x.pdf");
        if cfg!(windows) {
            assert_eq!(arg(r"C:\x\a.mp4"), r"C:\x\a.mp4");
            assert_eq!(arg("C:a.mp4"), "C:a.mp4");
        } else {
            assert_eq!(arg("/tmp/-x:y.mp4"), "/tmp/-x:y.mp4");
        }
        assert_eq!(magick_frame(Path::new("-a%03d.png"), 0), "./-a%%03d.png[0]");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = Path::new(OsStr::from_bytes(b"-\xff.mp4"));
            assert_eq!(
                path_arg_os(raw).as_bytes(),
                b"./-\xff.mp4",
                "not made lossy"
            );
        }
    }

    #[test]
    fn qpdf_is_given_its_arguments_on_stdin_only_for_a_wildcard_on_windows() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(!qpdf_wants_args_on_stdin(&args(&[
            "--show-npages",
            "C:\\docs\\a.pdf"
        ])));
        assert_eq!(
            qpdf_wants_args_on_stdin(&args(&["--show-npages", "\\\\?\\C:\\docs\\a.pdf"])),
            cfg!(windows)
        );
    }

    /// The stdin route against a real qpdf, which is how a Windows run
    /// with long paths reaches it.
    #[test]
    #[ignore = "requires qpdf; run with --ignored"]
    fn qpdf_reads_its_arguments_from_stdin() {
        let qpdf = crate::Resolver::new()
            .resolve(crate::Backend::Qpdf)
            .expect("qpdf")
            .path;
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample.pdf");
        let out = qpdf_with_args_on_stdin(
            &qpdf,
            &[OsString::from("--show-npages"), sample.into_os_string()],
        )
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3");
    }

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
