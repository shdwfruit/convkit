//! `--paste` for the commands that take a file: what the clipboard holds,
//! as a file they can read.

use std::path::PathBuf;

use convkit_core::{ConvError, ErrorCode};

use crate::clipboard::{self, Got};

/// How a pasted picture is named in messages, rather than by the passing
/// folder it is read from.
pub const PASTED: &str = "the pasted picture";

/// A file taken from the clipboard. A copied picture has no file of its
/// own, so it is written as clipboard.EXT into a folder that lives as long
/// as this does.
pub struct Pasted {
    pub path: PathBuf,
    pub folder: Option<tempfile::TempDir>,
}

impl Pasted {
    /// Whether this is a picture with no folder of its own, whose results
    /// go to the current folder rather than beside it.
    pub fn homeless(&self) -> bool {
        self.folder.is_some()
    }
}

fn refused(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// What the clipboard holds, read as `--paste` reads it.
pub fn get() -> Result<Got, ConvError> {
    clipboard::get().map_err(|e| refused(format!("couldn't paste: {e}")))
}

/// Writes a pasted picture as clipboard.EXT in a folder only this user can
/// open.
pub fn keep(format: convkit_core::Format, bytes: &[u8]) -> Result<Pasted, ConvError> {
    let (folder, path) = crate::output::private_temp(&format!("clipboard.{}", format.ext()))?;
    std::fs::write(&path, bytes)
        .map_err(|e| refused(format!("couldn't keep the pasted picture: {e}")))?;
    Ok(Pasted {
        path,
        folder: Some(folder),
    })
}

/// The one file `command` works on: a copied file, or a copied picture.
pub fn one(command: &str) -> Result<Pasted, ConvError> {
    from(get()?, command)
}

/// `one`, from what was already read.
pub fn from(got: Got, command: &str) -> Result<Pasted, ConvError> {
    match got {
        Got::Files(files) => match files.as_slice() {
            [one] => Ok(Pasted {
                path: one.clone(),
                folder: None,
            }),
            many => Err(refused(format!(
                "the clipboard holds {} files; {command} takes one",
                many.len()
            ))),
        },
        Got::Picture { format, bytes } => keep(format, &bytes),
        Got::Text(_) => Err(refused(
            "the clipboard holds text, not a picture or a file".to_string(),
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_picture_is_kept_where_only_this_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let kept = keep(convkit_core::Format::Png, b"x").unwrap();
        let folder = kept.path.parent().unwrap();
        let mode = std::fs::metadata(folder).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
