//! Writing a file a command drew itself (`conv qr`, `conv ascii`), and
//! turning a drawn PNG into another format the way `conv a.png b.jpg`
//! would.

use std::path::{Path, PathBuf};

use convkit_core::{exec, ConvError, ErrorCode, Format, Outcome, Request, Tuning};

use crate::cli::Cli;
use crate::commands::install;
use crate::install_prompt;

/// Writes `bytes` beside `path` and renames it into place, so a failed
/// write never leaves half a file under the name asked for.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<Outcome, ConvError> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.conv-{}", std::process::id()));
    let failed = |e: std::io::Error| {
        let _ = std::fs::remove_file(&temp);
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", path.display()),
        )
    };
    std::fs::write(&temp, bytes).map_err(failed)?;
    std::fs::rename(&temp, path).map_err(failed)?;
    Ok(Outcome {
        output: path.to_path_buf(),
        bytes: bytes.len() as u64,
        warnings: vec![],
        notes: vec![],
        backend_output: vec![],
        backends: vec![],
        remuxed: false,
        elapsed_ms: 0,
        sizing: None,
        enlarged: None,
        range: None,
        speed: None,
    })
}

/// Creates `-o`'s directory when one was given.
pub fn ensure_dir(dir: Option<&Path>) -> Result<(), ConvError> {
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            ConvError::new(
                ErrorCode::InvalidInvocation,
                format!("cannot create output directory {}: {e}", dir.display()),
            )
        })?;
    }
    Ok(())
}

/// Converts `input` to `output` as `conv input output` would, offering to
/// install a missing backend the same way.
pub fn convert(
    cli: &Cli,
    from: Format,
    input: &Path,
    to: Format,
    output: &Path,
    overwrite: bool,
) -> Result<Outcome, ConvError> {
    let req = Request {
        from,
        to,
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite,
        tuning: Tuning::default(),
        allow_extreme: false,
    };
    let mut result = exec::run(&req, &cli.resolver(), &mut |_| {});
    if let Err(e) = &result {
        if let (ErrorCode::BackendMissing, Some(backend)) = (e.code, e.backend) {
            if install_prompt::should_install(cli, backend)
                && install::install_backend(cli, backend).is_ok()
            {
                result = exec::run(&req, &cli.resolver(), &mut |_| {});
            }
        }
    }
    result
}

/// A path named `name` in a new folder only this user can open, for a
/// file handed to a backend: nobody else can read it or put something in
/// its place. The folder goes when the `TempDir` is dropped.
pub fn private_temp(name: &str) -> Result<(tempfile::TempDir, PathBuf), ConvError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("conv-");
    // Owner only; elsewhere the temp folder is per user already.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir().map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot make a temporary folder: {e}"),
        )
    })?;
    let path = dir.path().join(name);
    Ok((dir, path))
}

/// Converts a drawn PNG to `format` at `path`, through a temporary file.
pub fn convert_png(
    cli: &Cli,
    png: &[u8],
    path: &Path,
    format: Format,
    overwrite: bool,
) -> Result<Outcome, ConvError> {
    let (_dir, temp) = private_temp("drawn.png")?;
    std::fs::write(&temp, png).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write {}: {e}", temp.display()),
        )
    })?;
    convert(cli, Format::Png, &temp, format, path, overwrite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_files_go_in_a_private_folder() {
        let (dir, path) = private_temp("picture.png").unwrap();
        assert_eq!(path.parent(), Some(dir.path()));
        assert_eq!(path.file_name().unwrap(), "picture.png");
        assert_ne!(dir.path(), std::env::temp_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "only the owner can see in: {mode:o}");
        }
        let kept = dir.path().to_path_buf();
        drop(dir);
        assert!(!kept.exists(), "removed when dropped");
    }
}
