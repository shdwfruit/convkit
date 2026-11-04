//! Gathering what `pack` puts in an archive.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use super::read::plural;
use super::{failed, input_not_found};
use crate::{ConvError, ErrorCode, Result};

pub(crate) enum Source {
    /// A file or folder the user named: it goes in under its own name.
    Named(PathBuf),
    /// Everything inside a folder, at the archive's top: `convert`'s
    /// scratch folder.
    ContentsOf(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemKind {
    File { size: u64 },
    Dir,
    Symlink(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct Item {
    /// The name in the archive, `/`-separated, no trailing slash.
    pub name: String,
    pub path: PathBuf,
    pub kind: ItemKind,
    pub mode: Option<u32>,
    pub mtime: SystemTime,
}

#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub items: Vec<Item>,
    pub finder_skipped: u64,
    pub other_skipped: u64,
    /// Links that point outside what is packed, by archive name.
    pub outside_links: Vec<String>,
    /// Names that are not valid UTF-8, which a zip stores as the closest
    /// UTF-8 spelling.
    pub lossy_names: u64,
}

impl Collected {
    pub(crate) fn content_bytes(&self) -> u64 {
        self.items
            .iter()
            .map(|i| match i.kind {
                ItemKind::File { size } => size,
                _ => 0,
            })
            .sum()
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.finder_skipped > 0 {
            notes.push(format!(
                "Left out {} that only Finder uses (.DS_Store, ._ and __MACOSX).",
                plural(self.finder_skipped, "file", "files")
            ));
        }
        if self.other_skipped > 0 {
            notes.push(format!(
                "Left out {} that {} a file, folder or link (a socket, pipe or device).",
                plural(self.other_skipped, "item", "items"),
                if self.other_skipped == 1 {
                    "isn't"
                } else {
                    "aren't"
                }
            ));
        }
        if let Some(first) = self.outside_links.first() {
            let more = self.outside_links.len() - 1;
            let others = if more > 0 {
                format!(" and {}", plural(more as u64, "other", "others"))
            } else {
                String::new()
            };
            notes.push(format!(
                "Kept {first}{others} as a link to something outside what was packed; \
                 it won't work on another computer, and conv won't unpack it."
            ));
        }
        if self.lossy_names > 0 {
            notes.push(format!(
                "{} not UTF-8; a zip can only store {} with the closest UTF-8 spelling.",
                plural(self.lossy_names, "name is", "names are"),
                if self.lossy_names == 1 { "it" } else { "them" }
            ));
        }
        notes
    }
}

/// Finder's own files: `.DS_Store` (folder view settings), `__MACOSX`
/// (Finder's copy of extra file info in a zip), and `._name`, the
/// AppleDouble copy of `name`'s extra info written on disks that can't
/// hold it, only when `name` itself is beside it.
fn is_finder_file(dir: &Path, name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    if name == ".DS_Store" || name == "__MACOSX" {
        return true;
    }
    name.strip_prefix("._")
        .is_some_and(|real| !real.is_empty() && dir.join(real).symlink_metadata().is_ok())
}

/// What not to pack: the archive being written and its temporary file,
/// when they sit inside a folder being packed.
pub(crate) struct Skip {
    pub dir: Option<PathBuf>,
    pub names: Vec<std::ffi::OsString>,
}

impl Skip {
    /// The names to skip in `dir`: none unless it is the output's folder.
    fn in_dir(&self, dir: &Path) -> &[std::ffi::OsString] {
        match &self.dir {
            Some(d) if std::fs::canonicalize(dir).is_ok_and(|c| &c == d) => &self.names,
            _ => &[],
        }
    }
}

pub(crate) fn collect(sources: &[Source], skip: &Skip) -> Result<Collected> {
    let mut out = Collected::default();
    let mut tops: Vec<String> = Vec::new();
    for source in sources {
        match source {
            Source::ContentsOf(dir) => walk_dir(dir, "", 0, skip, &mut out)?,
            Source::Named(path) => {
                let meta = std::fs::metadata(path).map_err(|_| input_not_found(path))?;
                let name = top_name(path)?;
                if tops.contains(&name) {
                    return Err(ConvError::new(
                        ErrorCode::InvalidInvocation,
                        format!(
                            "two inputs would both be {name} in the archive; rename one, \
                             or put them in a folder and pack that"
                        ),
                    ));
                }
                tops.push(name.clone());
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if meta.is_dir() {
                    out.items.push(Item {
                        name: name.clone(),
                        path: path.clone(),
                        kind: ItemKind::Dir,
                        mode: mode_of(&meta),
                        mtime,
                    });
                    walk_dir(path, &name, 1, skip, &mut out)?;
                } else if meta.is_file() {
                    out.items.push(Item {
                        name,
                        path: path.clone(),
                        kind: ItemKind::File { size: meta.len() },
                        mode: mode_of(&meta),
                        mtime,
                    });
                } else {
                    return Err(failed(format!(
                        "{} is not a file or folder conv can pack",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// The name a named source goes in under: its own, or for `.` and the
/// like, the real folder's.
fn top_name(path: &Path) -> Result<String> {
    let named = match path.components().next_back() {
        Some(Component::Normal(n)) => Some(n.to_os_string()),
        _ => None,
    };
    let name = match named {
        Some(n) => n,
        None => std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_os_string()))
            .ok_or_else(|| {
                ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!("{} has no name to pack it under", path.display()),
                )
            })?,
    };
    Ok(name.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

fn walk_dir(
    dir: &Path,
    prefix: &str,
    depth: usize,
    skip: &Skip,
    out: &mut Collected,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| failed(format!("cannot read {}: {e}", dir.display())))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let skipped = skip.in_dir(dir);
    for entry in entries {
        let os_name = entry.file_name();
        if is_finder_file(dir, &os_name) {
            out.finder_skipped += 1;
            continue;
        }
        if skipped.contains(&os_name) {
            continue;
        }
        let part = match os_name.to_str() {
            Some(s) => s.to_string(),
            None => {
                out.lossy_names += 1;
                os_name.to_string_lossy().into_owned()
            }
        };
        let name = if prefix.is_empty() {
            part
        } else {
            format!("{prefix}/{part}")
        };
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let mode = mode_of(&meta);
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|e| failed(format!("cannot read the link {}: {e}", path.display())))?;
            if points_outside(&target, depth) {
                out.outside_links.push(name.clone());
            }
            out.items.push(Item {
                name,
                path,
                kind: ItemKind::Symlink(target),
                mode,
                mtime,
            });
        } else if meta.is_dir() {
            out.items.push(Item {
                name: name.clone(),
                path: path.clone(),
                kind: ItemKind::Dir,
                mode,
                mtime,
            });
            walk_dir(&path, &name, depth + 1, skip, out)?;
        } else if meta.is_file() {
            out.items.push(Item {
                name,
                path,
                kind: ItemKind::File { size: meta.len() },
                mode,
                mtime,
            });
        } else {
            out.other_skipped += 1;
        }
    }
    Ok(())
}

/// Whether a link `depth` folders below the archive's top points above
/// it, or anywhere absolute.
fn points_outside(target: &Path, depth: usize) -> bool {
    let mut level = depth as i64;
    for c in target.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => return true,
            Component::ParentDir => {
                level -= 1;
                if level < 0 {
                    return true;
                }
            }
            Component::Normal(_) => level += 1,
            Component::CurDir => {}
        }
    }
    false
}
