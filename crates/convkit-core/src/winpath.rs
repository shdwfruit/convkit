//! Windows path rules that `std::path` does not enforce.
//!
//! `Path` is a cross-platform value type: it will happily hold a name the
//! Windows filesystem layer refuses to treat as a file, and hand it to a
//! backend that then blocks forever or fails with an error naming the wrong
//! cause. The rules live here rather than inline in `exec` because they are
//! filesystem policy, not execution logic, and because they are testable on
//! every platform as pure functions -- only their *application* is gated on
//! actually running under Windows.

use std::path::Path;

/// The DOS-era device names Windows still intercepts before the filesystem
/// ever sees them. Opening `aux.jpg` opens the AUX device, so a backend
/// writing there blocks on a device that never accepts the write.
///
/// The set is exactly what this machine (Windows 10 19045) enforces, checked
/// by trying to create each one: `CON`/`PRN`/`AUX`/`NUL` and `COM1`-`COM9`/
/// `LPT1`-`LPT9` are intercepted, while `COM0` and `LPT0` -- which some
/// documentation lists as reserved -- create ordinary files. They are
/// deliberately left out: rejecting a name that works would be a new bug
/// rather than a fix for this one.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The reserved device name `path`'s final component denotes, if any.
///
/// Matching follows what Windows itself does, verified against real
/// `CreateFile` calls rather than inferred:
///
/// * case-insensitive -- `AUX.JPG` is the AUX device;
/// * only the part before the *first* dot counts, so `aux.tar.gz` is the
///   device as surely as `aux.jpg` is;
/// * trailing spaces and dots are stripped first, so `aux .jpg` is too.
///
/// Pure and platform-independent so it can be tested everywhere; see
/// [`check_output_name`] for the Windows-only application.
pub fn reserved_device_name(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = name.split('.').next().unwrap_or_default();
    let stem = stem.trim_end_matches([' ', '.']);
    RESERVED
        .iter()
        .copied()
        .find(|reserved| reserved.eq_ignore_ascii_case(stem))
}

/// Refuses an output path whose name Windows would resolve to a device, with
/// a message that names the reserved word.
///
/// Called before any backend is spawned, because the failure it prevents is
/// not a clean error: `conv photo.heic aux.jpg` used to hang indefinitely
/// with magick blocked on the AUX device, and killing `conv` left both an
/// orphaned `magick.exe` and an uncleaned scratch directory behind, since
/// `ScratchGuard`'s `Drop` never ran. `con.gif` and `nul.gif` instead streamed
/// the whole result into the void and reported `ffmpeg produced no output`,
/// and `prn.pdf` failed at the rename with `The system cannot find the file
/// specified. (os error 2)` -- three different misleading endings for one
/// cause (F197).
///
/// A no-op off Windows, where these are ordinary file names: a directory
/// synced from a Mac or a NAS may genuinely contain `con.heic`, and
/// converting it there must keep working.
pub fn check_output_name(path: &Path) -> crate::Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    match reserved_device_name(path) {
        None => Ok(()),
        Some(reserved) => Err(crate::ConvError::new(
            crate::ErrorCode::InvalidInvocation,
            format!(
                "{} is a reserved Windows device name, so {} cannot be a file; \
                 choose another output name",
                reserved,
                path.display()
            ),
        )),
    }
}

/// The path length at which a Windows path gets the `\\?\` treatment.
///
/// 248 is `MAX_PATH` (260) minus the 12 characters Win32 historically
/// reserves for an 8.3 file name -- the same cutoff Rust's own standard
/// library uses before it switches a path to verbatim form internally. Using
/// it here means every ordinary conversion renders exactly the argv it
/// rendered before, and only a path already close to the limit is rewritten.
const VERBATIM_THRESHOLD: usize = 248;

/// Whether `path` is long enough to be worth handing to a backend in
/// verbatim form. Measured on the *absolute* path, since a short relative
/// token in a deep working directory is a long path to the process that
/// opens it. Always false off Windows, where no such limit exists.
pub fn is_long(path: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    absolute_string(path).is_some_and(|abs| abs.chars().count() >= VERBATIM_THRESHOLD)
}

/// `path` in extended-length (`\\?\`) form, or `None` if it is already
/// verbatim, is not a shape that can be made verbatim, or we are not on
/// Windows.
///
/// Backends are not long-path aware even where the OS is: with
/// `LongPathsEnabled=1` set on this machine, magick still reports `unable to
/// open image` and soffice still reports `no export filter for  found` once
/// the path passes 260 characters, neither of which mentions path length
/// (F193). Every backend convkit drives -- ffmpeg, pandoc, typst and soffice
/// checked here, magick in the report -- accepts the verbatim form and
/// succeeds on a path that fails without it.
///
/// The scratch directory cannot simply move somewhere shorter: it sits
/// inside the destination precisely so the closing rename is atomic, which
/// is what makes Ctrl-C safe. Rewriting the argv is the fix that keeps both.
pub fn to_verbatim(path: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    prefix_verbatim(&absolute_string(path)?)
}

/// `path` made absolute and rendered back to a string, or `None` if it
/// cannot be (an empty path, or one that is not valid Unicode).
///
/// `std::path::absolute` is what makes the prefix safe to apply: a verbatim
/// path is passed to the filesystem *without* normalisation, so `.`, `..`
/// and forward slashes inside one are taken literally and resolve to
/// nothing. Absolutising first removes all three.
fn absolute_string(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let abs = std::path::absolute(path).ok()?;
    abs.to_str().map(str::to_owned)
}

/// The prefix rule itself, over an already-absolute Windows-form path.
///
/// Split out as pure string work so the cases that matter can be tested on
/// every platform rather than only on Windows:
///
/// * a drive path `C:\dir\file` becomes `\\?\C:\dir\file`;
/// * a UNC path `\\server\share\file` becomes `\\?\UNC\server\share\file`
///   -- the plain prefix is invalid for UNC and would address a different
///   thing rather than the same thing spelled longer;
/// * anything else -- already-verbatim paths, device paths, and any shape
///   not recognised -- is left exactly as it is, because a wrong rewrite
///   fails in a way that is harder to read than the original problem.
fn prefix_verbatim(absolute: &str) -> Option<String> {
    if absolute.starts_with(VERBATIM_PREFIX) || absolute.starts_with(DEVICE_PREFIX) {
        return None;
    }
    if let Some(rest) = absolute.strip_prefix(UNC_PREFIX) {
        return Some(format!("{VERBATIM_PREFIX}UNC\\{rest}"));
    }
    let b = absolute.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        return Some(format!("{VERBATIM_PREFIX}{absolute}"));
    }
    None
}

const VERBATIM_PREFIX: &str = "\\\\?\\";
const DEVICE_PREFIX: &str = "\\\\.\\";
const UNC_PREFIX: &str = "\\\\";

/// The sentence appended to a failed step's error when a path in it was past
/// Windows' limit, or `None` when that was not the case.
///
/// Backends report a long-path failure as something else entirely -- a
/// missing input, a missing export filter -- so when one fails with a path
/// this long, the length is worth naming even though [`to_verbatim`] should
/// already have prevented it. Deliberately phrased as an observation rather
/// than a diagnosis: it is a fact about the command, not a claim about the
/// cause.
pub fn long_path_note<'a>(paths: impl Iterator<Item = &'a str>) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let longest = paths.map(|p| p.chars().count()).max()?;
    (longest >= 260).then(|| {
        format!(
            "the longest path in this command is {longest} characters, past the \
             260-character limit most Windows programs still have"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_device_names_are_reserved_with_or_without_an_extension() {
        for name in [
            "aux", "aux.jpg", "con.gif", "nul.gif", "prn.pdf", "lpt1.png",
        ] {
            assert!(reserved_device_name(Path::new(name)).is_some(), "{name}");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(reserved_device_name(Path::new("AUX.JPG")), Some("AUX"));
        assert_eq!(reserved_device_name(Path::new("Com1.png")), Some("COM1"));
    }

    /// Windows resolves the device from the text before the *first* dot, not
    /// the last, so a double extension does not escape it.
    #[test]
    fn only_the_text_before_the_first_dot_decides() {
        assert_eq!(reserved_device_name(Path::new("aux.tar.gz")), Some("AUX"));
    }

    /// Win32 strips trailing spaces and dots before resolving a name, so
    /// `aux .jpg` reaches the AUX device too -- confirmed by `CreateFile` on
    /// Windows 10 19045.
    #[test]
    fn trailing_spaces_and_dots_are_stripped_before_matching() {
        assert_eq!(reserved_device_name(Path::new("aux .jpg")), Some("AUX"));
        assert_eq!(reserved_device_name(Path::new("aux..jpg")), Some("AUX"));
    }

    /// The reserved set is closed, not a prefix rule: names that merely start
    /// with a device word are ordinary files.
    #[test]
    fn names_that_merely_start_with_a_device_word_are_fine() {
        for name in ["conx.jpg", "aux2.jpg", "nullify.png", "com.jpg"] {
            assert_eq!(reserved_device_name(Path::new(name)), None, "{name}");
        }
    }

    /// `COM0` and `LPT0` create ordinary files on Windows 10 19045 (verified
    /// by creating them), so rejecting them would break a working conversion.
    #[test]
    fn com0_and_lpt0_are_not_reserved() {
        assert_eq!(reserved_device_name(Path::new("com0.jpg")), None);
        assert_eq!(reserved_device_name(Path::new("lpt0.jpg")), None);
    }

    /// Only the final component matters; a directory called `aux` cannot
    /// exist on Windows anyway, and a legitimate one elsewhere must not
    /// poison the file name inside it.
    #[test]
    fn only_the_final_component_is_examined() {
        assert_eq!(reserved_device_name(Path::new("aux/photo.jpg")), None);
        assert_eq!(
            reserved_device_name(Path::new("photos/aux.jpg")),
            Some("AUX")
        );
    }

    #[test]
    fn a_path_with_no_file_name_matches_nothing() {
        assert_eq!(reserved_device_name(Path::new("..")), None);
        assert_eq!(reserved_device_name(Path::new("")), None);
    }

    #[test]
    #[cfg(windows)]
    fn check_refuses_a_reserved_output_and_names_the_device() {
        let err = check_output_name(Path::new("out/aux.jpg")).unwrap_err();
        assert_eq!(err.code, crate::ErrorCode::InvalidInvocation);
        assert!(err.message.contains("AUX"), "{}", err.message);
    }

    #[test]
    #[cfg(windows)]
    fn check_accepts_an_ordinary_output() {
        assert!(check_output_name(Path::new("out/photo.jpg")).is_ok());
    }

    /// Off Windows these are ordinary names -- a folder synced from a Mac may
    /// genuinely hold `con.heic`, and converting it must keep working.
    #[test]
    #[cfg(not(windows))]
    fn check_is_a_no_op_off_windows() {
        assert!(check_output_name(Path::new("aux.jpg")).is_ok());
    }

    // --- extended-length path rewriting (F193) -------------------------------

    #[test]
    fn a_drive_path_gets_the_plain_verbatim_prefix() {
        assert_eq!(
            prefix_verbatim("C:\\dir\\photo.jpg").as_deref(),
            Some("\\\\?\\C:\\dir\\photo.jpg")
        );
    }

    /// A UNC path needs the `UNC\` infix: the plain prefix on
    /// `\\server\share` addresses a device called `server`, which is a
    /// different thing entirely rather than a longer spelling of the same one.
    #[test]
    fn a_unc_path_gets_the_unc_verbatim_prefix() {
        assert_eq!(
            prefix_verbatim("\\\\server\\share\\photo.jpg").as_deref(),
            Some("\\\\?\\UNC\\server\\share\\photo.jpg")
        );
    }

    #[test]
    fn an_already_verbatim_or_device_path_is_left_alone() {
        assert_eq!(prefix_verbatim("\\\\?\\C:\\dir\\photo.jpg"), None);
        assert_eq!(prefix_verbatim("\\\\?\\UNC\\server\\share\\p.jpg"), None);
        assert_eq!(prefix_verbatim("\\\\.\\COM1"), None);
    }

    /// Anything whose shape is not recognised is returned unchanged rather
    /// than guessed at.
    #[test]
    fn an_unrecognised_shape_is_left_alone() {
        assert_eq!(prefix_verbatim("relative/path.jpg"), None);
        assert_eq!(prefix_verbatim("photo.jpg"), None);
        assert_eq!(prefix_verbatim(""), None);
    }

    #[test]
    #[cfg(windows)]
    fn to_verbatim_absolutises_before_prefixing() {
        let got = to_verbatim("photo.jpg").expect("a relative path resolves against the cwd");
        assert!(got.starts_with("\\\\?\\"), "{got}");
        assert!(got.ends_with("\\photo.jpg"), "{got}");
        // Verbatim paths are not normalised by the filesystem, so no `.` or
        // `..` component may survive into one.
        assert!(!got.contains("\\.\\") && !got.contains("\\..\\"), "{got}");
    }

    #[test]
    #[cfg(not(windows))]
    fn verbatim_rewriting_is_a_no_op_off_windows() {
        assert_eq!(to_verbatim("/tmp/photo.jpg"), None);
        assert!(!is_long(&"x".repeat(400)));
    }

    #[test]
    #[cfg(windows)]
    fn only_paths_at_the_threshold_are_treated_as_long() {
        let deep = format!("C:\\{}\\photo.jpg", "d".repeat(300));
        assert!(is_long(&deep));
        assert!(!is_long("C:\\dir\\photo.jpg"));
    }

    #[test]
    fn the_long_path_note_names_the_longest_path_and_only_fires_past_the_limit() {
        let short = ["a".repeat(10), "b".repeat(20)];
        assert_eq!(long_path_note(short.iter().map(String::as_str)), None);

        let long = ["a".repeat(10), "b".repeat(300)];
        let note = long_path_note(long.iter().map(String::as_str));
        if cfg!(windows) {
            assert!(note.expect("past the limit").contains("300"));
        } else {
            assert_eq!(note, None, "not a limit that exists off Windows");
        }
    }

    #[test]
    fn the_long_path_note_says_nothing_about_an_empty_command() {
        assert_eq!(long_path_note(std::iter::empty()), None);
    }
}
