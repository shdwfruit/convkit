//! Writing an archive's entries into a scratch folder without letting any
//! of them reach outside it.
//!
//! The rules, each refusing the whole archive (`ErrorCode::UnsafeArchive`)
//! rather than skipping one entry, since an archive that tries any of
//! them was made to:
//!
//! - A name with a `..` component, or an absolute one (`/etc/passwd`,
//!   `C:\x`), is refused before anything is written for it.
//! - Files and folders are all written before any link exists, so writing
//!   a file can never pass through a link the archive made.
//! - A link is never made inside another link, and once every link
//!   exists, each one is followed component by component, through the
//!   others, and must stay inside the folder. A link to `/`, to `../x`, or
//!   to `b/..` where `b` is a link to `.`, is refused.
//! - A hard link must name a file the archive already unpacked.
//!
//! Modes keep the archive's read and execute bits, always with read and
//! write for the owner, and never setuid, setgid or sticky.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use super::{failed, human_size, name_of, ASK_ABOVE_BYTES, ASK_ABOVE_RATIO};
use crate::{ConvError, ErrorCode, Result};

/// How much an archive grows when unpacked, for the question `unpack`
/// asks before a likely zip bomb.
#[derive(Debug, Clone, Serialize)]
pub struct Expansion {
    pub archive: PathBuf,
    /// The archive's own size.
    pub packed: u64,
    /// The size it unpacks to: the total its index lists, or, for a
    /// compressed tar that lists nothing, what has been written so far.
    pub unpacked: u64,
    /// False when `unpacked` is only what has been written so far.
    pub listed: bool,
}

impl Expansion {
    pub fn ratio(&self) -> u64 {
        self.unpacked / self.packed.max(1)
    }

    /// Whether this needs a yes.
    pub fn is_large(&self) -> bool {
        let (bytes, ratio) = limits();
        self.unpacked > bytes && self.unpacked > self.packed.max(1).saturating_mul(ratio)
    }

    pub fn warning(&self) -> String {
        let size = if self.listed {
            format!("unpacks to {}", human_size(self.unpacked))
        } else {
            format!("has unpacked {} so far", human_size(self.unpacked))
        };
        format!(
            "{} {size}, {} times its own size ({}). An archive that grows this much is \
             often a zip bomb, made to fill the disk.",
            name_of(&self.archive),
            self.ratio(),
            human_size(self.packed)
        )
    }

    pub fn refusal(&self) -> ConvError {
        ConvError::new(
            ErrorCode::ConfirmationRequired,
            format!(
                "{} grows more than {ASK_ABOVE_RATIO} times its size; not unpacked. \
                 Pass --yes to unpack it anyway",
                name_of(&self.archive)
            ),
        )
    }
}

#[cfg(test)]
thread_local! {
    /// Lower lines for a test, so a zip bomb can be a few kilobytes.
    pub(crate) static TEST_LIMITS: std::cell::Cell<Option<(u64, u64)>> =
        const { std::cell::Cell::new(None) };
}

fn limits() -> (u64, u64) {
    #[cfg(test)]
    if let Some(l) = TEST_LIMITS.with(|c| c.get()) {
        return l;
    }
    (ASK_ABOVE_BYTES, ASK_ABOVE_RATIO)
}

/// An entry's name as a path inside the destination: `Ok(None)` for one
/// that names the destination itself (`./`), `Err` with the reason for one
/// that would land outside it.
pub(crate) fn relative(name: &Path) -> std::result::Result<Option<PathBuf>, &'static str> {
    let mut out = PathBuf::new();
    for c in name.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => return Err("it is an absolute path"),
            Component::ParentDir => return Err("it climbs out with .."),
            Component::CurDir => {}
            Component::Normal(part) => {
                // `name:stream` writes an NTFS alternate data stream, not a
                // file; and Windows refuses `:` in a name anyway.
                if cfg!(windows) && part.to_string_lossy().contains(':') {
                    return Err("it names a Windows alternate data stream");
                }
                out.push(part);
            }
        }
    }
    Ok((!out.as_os_str().is_empty()).then_some(out))
}

pub(crate) fn unsafe_entry(archive: &Path, entry: &str, why: &str) -> ConvError {
    ConvError::new(
        ErrorCode::UnsafeArchive,
        format!(
            "{} has an entry that would land outside the folder it unpacks into ({entry}: {why}); \
             nothing was unpacked",
            name_of(archive)
        ),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
    Symlink(PathBuf),
    Hardlink(PathBuf),
    /// A device, FIFO or anything else that is not data: never made.
    Other,
}

/// One entry as a reader hands it over.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The name as stored, `/`-separated.
    pub name: PathBuf,
    pub kind: EntryKind,
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
    pub size: u64,
}

impl Entry {
    pub(crate) fn display(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }
}

/// Where a reader sends entries: `Extractor` writes them, `read::Lister`
/// only names them.
pub(crate) trait Sink {
    /// The total the archive's index lists, before the first entry, for a
    /// format that has one.
    fn listed(&mut self, _total: u64) -> Result<()> {
        Ok(())
    }
    /// False for a sink that only names entries, so a zip's names can be
    /// listed without its password.
    fn wants_data(&self) -> bool {
        true
    }
    fn entry(&mut self, entry: Entry, data: &mut dyn Read) -> Result<()>;
}

/// Writes entries under `root`, deferring links to `finish`.
pub(crate) struct Extractor<'a> {
    root: PathBuf,
    archive: PathBuf,
    packed: u64,
    written: u64,
    asked: bool,
    confirm: &'a mut dyn FnMut(&Expansion) -> bool,
    links: Vec<(PathBuf, PathBuf, String)>,
    hardlinks: Vec<(PathBuf, PathBuf, String)>,
    dirs: Vec<(PathBuf, Option<SystemTime>, Option<u32>)>,
    pub files: u64,
    pub folders: u64,
    pub link_count: u64,
    pub mac_skipped: u64,
    pub other_skipped: u64,
    pub windows_links_skipped: u64,
}

impl<'a> Extractor<'a> {
    pub(crate) fn new(
        root: &Path,
        archive: &Path,
        confirm: &'a mut dyn FnMut(&Expansion) -> bool,
    ) -> Self {
        Extractor {
            root: root.to_path_buf(),
            archive: archive.to_path_buf(),
            packed: std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0),
            written: 0,
            asked: false,
            confirm,
            links: Vec::new(),
            hardlinks: Vec::new(),
            dirs: Vec::new(),
            files: 0,
            folders: 0,
            link_count: 0,
            mac_skipped: 0,
            other_skipped: 0,
            windows_links_skipped: 0,
        }
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    fn ask(&mut self, unpacked: u64, listed: bool) -> Result<()> {
        if self.asked {
            return Ok(());
        }
        let e = Expansion {
            archive: self.archive.clone(),
            packed: self.packed,
            unpacked,
            listed,
        };
        if !e.is_large() {
            return Ok(());
        }
        self.asked = true;
        if (self.confirm)(&e) {
            Ok(())
        } else {
            Err(e.refusal())
        }
    }

    /// Makes `rel`'s parent folders. None of them can be a link: links are
    /// only made in `finish`.
    fn parent_of(&self, rel: &Path) -> Result<PathBuf> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                failed(format!(
                    "cannot unpack {} from {}: {e}",
                    rel.display(),
                    name_of(&self.archive)
                ))
            })?;
        }
        Ok(path)
    }

    /// Clears a file a repeated entry left, so the later one wins, as tar
    /// does. A folder in the way is an error: the archive holds both.
    fn clear(&self, path: &Path, rel: &Path) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => Err(failed(format!(
                "{} holds {} as both a file and a folder",
                name_of(&self.archive),
                rel.display()
            ))),
            Ok(_) => std::fs::remove_file(path)
                .map_err(|e| failed(format!("cannot replace {}: {e}", path.display()))),
            Err(_) => Ok(()),
        }
    }

    fn write_file(&mut self, rel: &Path, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        let path = self.parent_of(rel)?;
        self.clear(&path, rel)?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| failed(format!("cannot write {}: {e}", path.display())))?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match data.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(&self.archive, &entry.display(), &e)),
            };
            self.written += n as u64;
            self.ask(self.written, false)?;
            out.write_all(&buf[..n])
                .map_err(|e| failed(format!("cannot write {}: {e}", path.display())))?;
        }
        drop(out);
        set_mode(&path, entry.mode, false);
        if let Some(t) = entry.mtime {
            let _ = filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(t));
        }
        self.files += 1;
        Ok(())
    }

    /// Makes every deferred link, checks each stays inside, then gives the
    /// folders their modes and times.
    pub(crate) fn finish(mut self) -> Result<Self> {
        for (rel, target, shown) in std::mem::take(&mut self.hardlinks) {
            let src = self.root.join(&target);
            if !std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
                return Err(failed(format!(
                    "{} links {shown} to {}, which it does not hold as a file",
                    name_of(&self.archive),
                    target.display()
                )));
            }
            let dst = self.parent_of(&rel)?;
            self.clear(&dst, &rel)?;
            std::fs::hard_link(&src, &dst)
                .or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))
                .map_err(|e| failed(format!("cannot write {}: {e}", dst.display())))?;
            self.files += 1;
        }
        let links = std::mem::take(&mut self.links);
        for (rel, target, shown) in &links {
            if relative_target_escapes(target) {
                return Err(unsafe_entry(&self.archive, shown, "it links outside"));
            }
            if self.has_link_above(rel) {
                return Err(unsafe_entry(
                    &self.archive,
                    shown,
                    "it sits inside another link",
                ));
            }
            let path = self.parent_of(rel)?;
            self.clear(&path, rel)?;
            if !make_link(target, &path)
                .map_err(|e| failed(format!("cannot make the link {}: {e}", path.display())))?
            {
                self.windows_links_skipped += 1;
                continue;
            }
            self.link_count += 1;
        }
        // Only now, with every link in place, can a link be followed
        // through the others.
        for (rel, _, shown) in &links {
            if std::fs::symlink_metadata(self.root.join(rel)).is_ok_and(|m| m.is_symlink())
                && !stays_inside(&self.root, rel)
            {
                return Err(unsafe_entry(&self.archive, shown, "it links outside"));
            }
        }
        let mut dirs = std::mem::take(&mut self.dirs);
        // Deepest first: setting a folder's time and then writing into it
        // would move the time again.
        dirs.sort_by_key(|(rel, _, _)| std::cmp::Reverse(rel.components().count()));
        for (rel, mtime, mode) in dirs {
            let path = self.root.join(&rel);
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            set_mode(&path, mode, true);
            if let Some(t) = mtime {
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(t));
            }
        }
        Ok(self)
    }

    fn has_link_above(&self, rel: &Path) -> bool {
        let mut path = self.root.clone();
        let mut parts = rel.components().peekable();
        while let Some(c) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            path.push(c);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
                return true;
            }
        }
        false
    }
}

impl Sink for Extractor<'_> {
    fn listed(&mut self, total: u64) -> Result<()> {
        self.ask(total, true)
    }

    fn entry(&mut self, entry: Entry, data: &mut dyn Read) -> Result<()> {
        let shown = entry.display();
        let rel = match relative(&entry.name) {
            Err(why) => return Err(unsafe_entry(&self.archive, &shown, why)),
            Ok(None) => return Ok(()),
            Ok(Some(rel)) => rel,
        };
        // Finder's copy of extra file info (resource forks, tags), which
        // every other unzip tool shows as clutter.
        if rel
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == "__MACOSX")
        {
            self.mac_skipped += 1;
            return Ok(());
        }
        match &entry.kind {
            EntryKind::File => self.write_file(&rel, &entry, data)?,
            EntryKind::Dir => {
                let path = self.root.join(&rel);
                if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
                    self.clear(&path, &rel)?;
                }
                std::fs::create_dir_all(&path)
                    .map_err(|e| failed(format!("cannot make {}: {e}", path.display())))?;
                self.dirs.push((rel, entry.mtime, entry.mode));
                self.folders += 1;
            }
            EntryKind::Symlink(target) => {
                self.links.push((rel, target.clone(), shown));
            }
            EntryKind::Hardlink(target) => {
                let target = match relative(target) {
                    Err(why) => return Err(unsafe_entry(&self.archive, &shown, why)),
                    Ok(None) => {
                        return Err(unsafe_entry(
                            &self.archive,
                            &shown,
                            "it links to the folder",
                        ))
                    }
                    Ok(Some(t)) => t,
                };
                self.hardlinks.push((rel, target, shown));
            }
            EntryKind::Other => self.other_skipped += 1,
        }
        Ok(())
    }
}

/// A read error inside an entry: a damaged archive, or, for a zip with
/// the old ZipCrypto lock, a wrong password that passed its one-byte check.
pub(crate) fn read_error(archive: &Path, entry: &str, e: &std::io::Error) -> ConvError {
    failed(format!(
        "cannot read {entry} from {}: {e}. The archive may be damaged, or the password wrong",
        name_of(archive)
    ))
}

/// A link target that is absolute, or climbs above the link's own top
/// folder on its own, is refused before it is made.
fn relative_target_escapes(target: &Path) -> bool {
    target.as_os_str().is_empty()
        || target
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
}

/// Follows the link at `rel` (under `root`) one component at a time, and
/// through any link it meets, the way the system will. False as soon as it
/// would leave `root`, or after 40 links (a loop).
fn stays_inside(root: &Path, rel: &Path) -> bool {
    let mut at: Vec<OsString> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let Ok(first) = std::fs::read_link(root.join(rel)) else {
        return false;
    };
    let owned = |p: &Path| -> Vec<OsString> {
        p.components()
            .map(|c| match c {
                Component::ParentDir => OsString::from(".."),
                Component::CurDir => OsString::from("."),
                Component::Normal(n) => n.to_os_string(),
                _ => OsString::from("/"),
            })
            .collect()
    };
    if first.has_root()
        || first
            .components()
            .any(|c| matches!(c, Component::Prefix(_)))
    {
        return false;
    }
    let mut todo: VecDeque<OsString> = owned(&first).into();
    let mut hops = 0;
    while let Some(part) = todo.pop_front() {
        if part == "/" {
            return false;
        } else if part == "." {
            continue;
        } else if part == ".." {
            if at.pop().is_none() {
                return false;
            }
            continue;
        }
        at.push(part);
        let here: PathBuf = at.iter().fold(root.to_path_buf(), |p, c| p.join(c));
        if std::fs::symlink_metadata(&here).is_ok_and(|m| m.is_symlink()) {
            hops += 1;
            if hops > 40 {
                return false;
            }
            let Ok(target) = std::fs::read_link(&here) else {
                return false;
            };
            if target.has_root()
                || target
                    .components()
                    .any(|c| matches!(c, Component::Prefix(_)))
            {
                return false;
            }
            at.pop();
            for c in owned(&target).into_iter().rev() {
                todo.push_front(c);
            }
        }
    }
    true
}

/// Makes a symbolic link. False where links cannot be made: Windows
/// without Developer Mode, which conv says in a note rather than fail on.
fn make_link(target: &Path, path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path)?;
        Ok(true)
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, path) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(1314) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The archive's read and execute bits, read and write for the owner
/// always, and none of setuid, setgid, sticky or group/other write.
fn set_mode(path: &Path, mode: Option<u32>, dir: bool) {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let owner = if dir { 0o700 } else { 0o600 };
        let mode = (mode & 0o755) | owner;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode, dir);
}

/// The archive's download mark, `com.apple.quarantine`, if it has one.
#[cfg(target_os = "macos")]
pub(crate) fn quarantine_of(archive: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(archive.as_os_str().as_bytes()).ok()?;
    let name = c"com.apple.quarantine";
    let mut buf = vec![0u8; 1024];
    // SAFETY: both strings are NUL-terminated and `buf` is as long as said.
    let n = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            0,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn quarantine_of(_archive: &Path) -> Option<Vec<u8>> {
    None
}

/// Marks everything under `root` (and `root`) with the archive's download
/// mark, as Archive Utility does, so Gatekeeper still checks an app that
/// came inside a downloaded zip.
#[cfg(target_os = "macos")]
pub(crate) fn mark_downloaded(root: &Path, value: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    fn mark(path: &Path, value: &[u8]) {
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: NUL-terminated strings; `value` is as long as said.
        unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            );
        }
    }
    fn walk(path: &Path, value: &[u8]) {
        mark(path, value);
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for e in entries.flatten() {
                    walk(&e.path(), value);
                }
            }
        }
    }
    walk(root, value);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn mark_downloaded(_root: &Path, _value: &[u8]) {}
