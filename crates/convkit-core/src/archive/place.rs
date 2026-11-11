//! Moving an unpacked archive from its scratch folder to where it goes.
//!
//! Nothing that exists is overwritten without `overwrite`. Unpacking into
//! a folder that exists adds to it: every name the archive would put
//! there is checked first, and if any is taken, nothing is moved. With
//! `overwrite`, a file in the way is replaced; a folder is never replaced
//! by a file, or a file by a folder, since that would delete more than
//! the one name.

use std::path::{Path, PathBuf};

use super::{
    failed, name_of, read, scratch_name, ArchiveFormat, Destination, Expansion, Report, Scratch,
    UnpackOptions,
};
use crate::{ConvError, ErrorCode, Result};

pub(crate) fn unpack(
    archive: &Path,
    format: ArchiveFormat,
    dest: &Destination,
    opts: &UnpackOptions,
    confirm: &mut dyn FnMut(&Expansion) -> bool,
) -> Result<Report> {
    // The scratch folder sits beside the result so the final move is a
    // rename on one disk, never a copy.
    // The folders this run makes on the way, removed again if it fails.
    let mut made = Scratch {
        path: PathBuf::new(),
        keep: false,
    };
    let (work_dir, into) = match dest {
        Destination::Beside(dir) => (dir.clone(), None),
        Destination::Into(out) if out.is_dir() => {
            // A link to a folder: `move_into` looks at the link itself.
            let out = out.canonicalize().unwrap_or_else(|_| out.clone());
            (out.clone(), Some(out))
        }
        Destination::Into(out) => {
            if std::fs::symlink_metadata(out).is_ok() {
                return Err(ConvError::new(
                    ErrorCode::OutputExists,
                    format!("{} exists and is not a folder", out.display()),
                ));
            }
            let parent = super::parent_dir(out);
            made.path = parent
                .ancestors()
                .take_while(|a| !a.exists())
                .last()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            std::fs::create_dir_all(&parent)
                .map_err(|e| failed(format!("cannot make the folder {}: {e}", parent.display())))?;
            (parent, Some(out.clone()))
        }
    };
    if !work_dir.is_dir() {
        return Err(failed(format!(
            "output directory does not exist: {}",
            work_dir.display()
        )));
    }
    let scratch = Scratch {
        path: work_dir.join(scratch_name("unpack")),
        keep: false,
    };
    std::fs::create_dir(&scratch.path).map_err(|e| {
        failed(format!(
            "cannot make a scratch folder in {}: {e}",
            work_dir.display()
        ))
    })?;
    let opened = read::extract(
        archive,
        format,
        &scratch.path,
        opts.password.as_deref(),
        confirm,
    )?;

    // What moves, and to where.
    let beside = into.is_none();
    let (from, to) = match into {
        Some(out) => (scratch.path.clone(), out),
        None => {
            let top = children(&scratch.path)?;
            match top.as_slice() {
                [only] => (scratch.path.join(only), work_dir.join(only)),
                _ => {
                    let stem = ArchiveFormat::stem(archive).unwrap_or_else(|| "unpacked".into());
                    (scratch.path.clone(), work_dir.join(stem))
                }
            }
        }
    };
    let mut notes = opened.notes;
    let (replaced, merged) = move_into(&from, &to, opts.overwrite)?;
    if merged && beside {
        notes.push(format!(
            "{} already existed; added what the archive holds to it.",
            name_of(&to)
        ));
    }
    if replaced > 0 {
        notes.push(format!(
            "Replaced {} already there.",
            crate::tools::plural(replaced, "file", "files")
        ));
    }
    made.keep = true;
    let bytes = opened.bytes;
    Ok(Report {
        output: to,
        format,
        files: opened.files,
        folders: opened.folders,
        links: opened.links,
        bytes,
        content_bytes: bytes,
        encrypted: opened.encrypted,
        notes,
    })
}

fn children(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| failed(format!("cannot read {}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    // macOS's `._X` copy of X's extended attributes, on disks without them.
    let all = names.clone();
    names.retain(|n| {
        n.to_string_lossy()
            .strip_prefix("._")
            .is_none_or(|x| !all.iter().any(|o| o == x))
    });
    Ok(names)
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Moves `from` to `to`. Returns how many files were replaced, and
/// whether `to` was a folder already there that it was added to.
fn move_into(from: &Path, to: &Path, overwrite: bool) -> Result<(u64, bool)> {
    let Ok(existing) = std::fs::symlink_metadata(to) else {
        std::fs::rename(from, to)
            .map_err(|e| failed(format!("cannot move into {}: {e}", to.display())))?;
        return Ok((0, false));
    };
    let from_dir = is_real_dir(from);
    match (from_dir, existing.is_dir()) {
        (true, true) => {
            let mut taken = Vec::new();
            let mut clash = None;
            conflicts(from, to, &mut taken, &mut clash);
            if let Some(c) = clash {
                return Err(c);
            }
            if !taken.is_empty() && !overwrite {
                taken.sort();
                let shown: Vec<String> = taken
                    .iter()
                    .take(3)
                    .map(|p| p.strip_prefix(to).unwrap_or(p).display().to_string())
                    .collect();
                let more = if taken.len() > 3 {
                    format!(" and {} more", taken.len() - 3)
                } else {
                    String::new()
                };
                return Err(ConvError::new(
                    ErrorCode::OutputExists,
                    format!(
                        "{} already holds {}{more}; pass -y to replace them",
                        to.display(),
                        shown.join(", ")
                    ),
                ));
            }
            let mut moved = Vec::new();
            if let Err(e) = merge(from, to, &mut moved) {
                // Half an archive in the folder is worse than none.
                for (src, dst) in moved.iter().rev() {
                    let _ = std::fs::rename(dst, src);
                }
                return Err(e);
            }
            Ok((taken.len() as u64, true))
        }
        (false, false) if overwrite => {
            std::fs::rename(from, to)
                .map_err(|e| failed(format!("cannot move into {}: {e}", to.display())))?;
            Ok((1, false))
        }
        (false, false) => Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", to.display()),
        )),
        (true, false) | (false, true) => Err(type_clash(to, existing.is_dir())),
    }
}

fn type_clash(to: &Path, is_dir: bool) -> ConvError {
    let what = if is_dir { "folder" } else { "file" };
    let other = if is_dir { "file" } else { "folder" };
    ConvError::new(
        ErrorCode::OutputExists,
        format!(
            "{} is a {what} and the archive holds a {other} there; move it away and try again",
            to.display()
        ),
    )
}

/// The files (and links) under `from` whose names are taken under `to`.
/// A folder on both sides is looked into, not counted. A file against a
/// folder is a clash no flag resolves.
fn conflicts(from: &Path, to: &Path, taken: &mut Vec<PathBuf>, clash: &mut Option<ConvError>) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for e in entries.flatten() {
        let target = to.join(e.file_name());
        let Ok(there) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        let here_dir = e.file_type().is_ok_and(|t| t.is_dir());
        match (here_dir, there.is_dir()) {
            (true, true) => conflicts(&e.path(), &target, taken, clash),
            (false, false) => taken.push(target),
            _ => {
                if clash.is_none() {
                    *clash = Some(type_clash(&target, there.is_dir()));
                }
            }
        }
    }
}

/// Moves everything in `from` into the folder `to`, replacing the files
/// `conflicts` found (already allowed). A link already in `to` is
/// replaced, never followed.
fn merge(from: &Path, to: &Path, moved: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(from)
        .map_err(|e| failed(format!("cannot read {}: {e}", from.display())))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let src = e.path();
        let dst = to.join(e.file_name());
        if is_real_dir(&src) && is_real_dir(&dst) {
            merge(&src, &dst, moved)?;
            continue;
        }
        std::fs::rename(&src, &dst)
            .map_err(|e| failed(format!("cannot move into {}: {e}", dst.display())))?;
        moved.push((src, dst));
    }
    Ok(())
}
