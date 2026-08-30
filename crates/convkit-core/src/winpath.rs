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
}
