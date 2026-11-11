//! Making and opening archives: zip, tar, and tar compressed with gzip,
//! xz, bzip2 or zstd, plus reading 7z. All of it runs in this process,
//! with no backend, so it works on a machine with nothing installed.
//!
//! Three operations, each writing nothing it cannot take back:
//!
//! - `pack` puts files and folders into a new archive, written beside its
//!   final name and renamed into place.
//! - `unpack` opens an archive into a scratch folder beside the
//!   destination and moves the result into place only once every entry
//!   has been written and checked. An entry that would land outside the
//!   destination (a `../` or absolute name, a link pointing out of it)
//!   fails the whole unpack with `ErrorCode::UnsafeArchive`, and the
//!   scratch folder is removed, so a hostile archive leaves nothing behind.
//! - `convert` unpacks into a scratch folder and packs that into the new
//!   format, with the same checks.
//!
//! Formats are told by the file name, as everywhere else in convkit, but
//! by its longest known suffix rather than its last extension, so
//! `backup.2026.tar.gz` is a gzipped tar named `backup.2026`.

mod place;
mod read;
mod safe;
mod time;
mod walk;
mod write;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use crate::{ConvError, ErrorCode, Remediation, Result};

pub use safe::Expansion;

/// Past this many bytes unpacked, an archive that grows more than
/// `ASK_ABOVE_RATIO` times its own size needs a yes. Ordinary archives
/// grow 2 to 10 times (text and logs up to about 50); a zip bomb grows
/// thousands of times, and a gigabyte is where filling a disk starts to
/// matter.
pub const ASK_ABOVE_BYTES: u64 = 1 << 30;
/// See `ASK_ABOVE_BYTES`.
pub const ASK_ABOVE_RATIO: u64 = 100;

/// The environment variable conv reads an archive password from, so a
/// script never has to put one on the command line.
pub const PASSWORD_ENV: &str = "CONVKIT_PASSWORD";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
    /// Read only: sevenz-rust2 can write 7z too, but a 7z made here would
    /// add nothing zip and tar.xz don't already give.
    SevenZ,
    /// Known only to refuse it with something better than "unknown
    /// format": there is no maintained, freely licensed RAR decoder in
    /// Rust, and the unrar sources forbid using them to make a RAR writer.
    Rar,
}

/// Every archive format and the suffixes it is known by, canonical first.
/// Lowercase, without the leading dot.
const TABLE: &[(ArchiveFormat, &[&str])] = &[
    (ArchiveFormat::Zip, &["zip"]),
    (ArchiveFormat::Tar, &["tar"]),
    (ArchiveFormat::TarGz, &["tar.gz", "tgz"]),
    (ArchiveFormat::TarXz, &["tar.xz", "txz"]),
    (ArchiveFormat::TarBz2, &["tar.bz2", "tbz2", "tbz"]),
    (ArchiveFormat::TarZst, &["tar.zst", "tzst"]),
    (ArchiveFormat::SevenZ, &["7z"]),
    (ArchiveFormat::Rar, &["rar"]),
];

impl Serialize for ArchiveFormat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.ext())
    }
}

impl ArchiveFormat {
    pub fn all() -> impl Iterator<Item = ArchiveFormat> {
        TABLE.iter().map(|(f, _)| *f)
    }

    /// The canonical suffix, without a leading dot: `zip`, `tar.gz`.
    pub fn ext(&self) -> &'static str {
        self.exts()[0]
    }

    /// Every suffix this format is known by, canonical first.
    pub fn exts(&self) -> &'static [&'static str] {
        TABLE
            .iter()
            .find(|(f, _)| f == self)
            .map(|(_, e)| *e)
            .expect("every ArchiveFormat has a TABLE row")
    }

    pub fn can_read(&self) -> bool {
        !matches!(self, ArchiveFormat::Rar)
    }

    pub fn can_write(&self) -> bool {
        !matches!(self, ArchiveFormat::Rar | ArchiveFormat::SevenZ)
    }

    /// `zip`, `.tar.gz`, `TGZ`: a format named on its own, as `--to` and
    /// the bare `.ext` shorthand name one.
    pub fn from_name(name: &str) -> Option<ArchiveFormat> {
        let needle = name.strip_prefix('.').unwrap_or(name).to_ascii_lowercase();
        TABLE
            .iter()
            .find(|(_, exts)| exts.contains(&needle.as_str()))
            .map(|(f, _)| *f)
    }

    /// The format a file name ends in, and how long that suffix is
    /// (without its dot). The longest suffix wins, so `a.tar.gz` is a
    /// gzipped tar rather than nothing, and a name that is only the
    /// suffix (`.zip`) is not an archive name at all: that spelling is the
    /// bare-extension shorthand.
    fn of_name(name: &str) -> Option<(ArchiveFormat, usize)> {
        let lower = name.to_ascii_lowercase();
        TABLE
            .iter()
            .flat_map(|(f, exts)| exts.iter().map(move |e| (*f, *e)))
            .filter(|(_, e)| {
                lower.len() > e.len() + 1
                    && lower.ends_with(e)
                    && lower.as_bytes()[lower.len() - e.len() - 1] == b'.'
            })
            .max_by_key(|(_, e)| e.len())
            .map(|(f, e)| (f, e.len()))
    }

    pub fn from_path(path: &Path) -> Option<ArchiveFormat> {
        let name = path.file_name()?.to_string_lossy();
        Self::of_name(&name).map(|(f, _)| f)
    }

    /// The name without the archive suffix: `photos.tar.gz` -> `photos`.
    pub fn stem(path: &Path) -> Option<String> {
        let name = path.file_name()?.to_string_lossy();
        let (_, len) = Self::of_name(&name)?;
        Some(name[..name.len() - len - 1].to_string())
    }

    /// The formats conv writes, as `zip, tar, tar.gz ...`.
    pub fn writable_list() -> String {
        Self::all()
            .filter(|f| f.can_write())
            .map(|f| f.ext())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The refusal for a format conv cannot open, naming what can.
    pub fn unreadable(path: &Path) -> ConvError {
        ConvError {
            code: ErrorCode::UnsupportedPair,
            message: format!(
                "conv can't open RAR archives like {}: there is no free RAR decoder it can build in",
                path.display()
            ),
            backend: None,
            remediation: Some(Remediation {
                managed: None,
                manual: Some(
                    "open it with The Unarchiver or Keka (Mac), 7-Zip (Windows), or unar (any system)"
                        .to_string(),
                ),
            }),
        }
    }

    /// The refusal for a format conv reads but does not write.
    pub fn unwritable(&self) -> ConvError {
        ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "conv opens {} archives but doesn't make them; it makes {}",
                self.ext(),
                Self::writable_list()
            ),
        )
    }
}

/// How `pack` and `convert` write.
#[derive(Debug, Clone, Default)]
pub struct PackOptions {
    pub overwrite: bool,
    /// Encrypt a zip with AES-256 under this password. Zip only.
    pub password: Option<String>,
}

/// How `unpack` and `convert` read.
#[derive(Debug, Clone, Default)]
pub struct UnpackOptions {
    pub overwrite: bool,
    /// The password for an encrypted zip or 7z.
    pub password: Option<String>,
}

/// Where `unpack` puts what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// In this folder, the way Finder opens an archive: an archive holding
    /// one file or folder unpacks as that item, and anything else as a
    /// folder named after the archive. `photos.zip` holding `photos/` gives
    /// `photos/`, never `photos/photos/`.
    Beside(PathBuf),
    /// Into exactly this folder, as `unzip -d` and `tar -C` do. It is made
    /// if missing; if it exists, the archive's items are added to it.
    Into(PathBuf),
}

/// What `pack`, `unpack` or `convert` made.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The archive written, or the file or folder unpacked.
    #[serde(serialize_with = "crate::lossy_path")]
    pub output: PathBuf,
    /// The archive's format: the one written, or the one opened.
    pub format: ArchiveFormat,
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    /// The size of what was written: the archive made, or everything
    /// unpacked.
    pub bytes: u64,
    /// The size of the files themselves, before packing or after unpacking.
    pub content_bytes: u64,
    /// The archive written is encrypted.
    pub encrypted: bool,
    /// Things to know about the result, for the frontend to show.
    pub notes: Vec<String>,
}

/// One entry, as `list` reads it.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
}

/// What an archive holds, read without writing anything: for `--dry-run`.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub format: ArchiveFormat,
    pub entries: Vec<Listed>,
    pub encrypted: bool,
}

impl Listing {
    /// The entries `unpack` would write: all but `__MACOSX`.
    pub fn kept(&self) -> impl Iterator<Item = &Listed> {
        self.entries.iter().filter(|e| {
            !e.name
                .trim_start_matches("./")
                .split(['/', '\\'])
                .next()
                .is_some_and(|top| top == "__MACOSX")
        })
    }

    /// The size `unpack` would write.
    pub fn total(&self) -> u64 {
        self.kept().map(|e| e.size).fold(0, u64::saturating_add)
    }

    pub fn files(&self) -> usize {
        self.kept().filter(|e| e.kind == "file").count()
    }

    /// The top-level names, in order, without repeats.
    pub fn top_level(&self) -> Vec<String> {
        let mut tops: Vec<String> = Vec::new();
        for e in &self.entries {
            let Ok(Some(rel)) = safe::relative(Path::new(&e.name)) else {
                continue;
            };
            let Some(first) = rel.components().next() else {
                continue;
            };
            let first = first.as_os_str().to_string_lossy().into_owned();
            if first != "__MACOSX" && !tops.contains(&first) {
                tops.push(first);
            }
        }
        tops
    }
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

/// A hidden name for scratch work in a folder, unique within this run.
fn scratch_name(what: &str) -> String {
    format!(
        ".convkit-{what}-{}-{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    )
}

/// Removes a scratch file or folder when dropped, unless kept. Removing a
/// folder never follows a link inside it.
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if m.is_dir() => {
                let _ = std::fs::remove_dir_all(&self.path);
            }
            Ok(_) => {
                let _ = std::fs::remove_file(&self.path);
            }
            Err(_) => {}
        }
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn failed(message: String) -> ConvError {
    ConvError::new(ErrorCode::ConversionFailed, message)
}

fn exists_error(path: &Path) -> ConvError {
    ConvError::new(
        ErrorCode::OutputExists,
        format!("{} exists; pass -y to overwrite", path.display()),
    )
}

/// The refusal for an encrypted archive opened without a password.
pub fn password_needed(archive: &Path) -> ConvError {
    ConvError {
        code: ErrorCode::PasswordProtected,
        message: format!("{} is password-protected", name_of(archive)),
        backend: None,
        remediation: Some(Remediation {
            managed: None,
            manual: Some(format!(
                "run it in a terminal to be asked for the password, or set {PASSWORD_ENV}"
            )),
        }),
    }
}

fn wrong_password(archive: &Path) -> ConvError {
    ConvError {
        code: ErrorCode::PasswordProtected,
        message: format!("wrong password for {}", name_of(archive)),
        backend: None,
        remediation: None,
    }
}

fn input_not_found(path: &Path) -> ConvError {
    ConvError::new(
        ErrorCode::InputNotFound,
        format!("input not found: {}", path.display()),
    )
}

fn readable_format(archive: &Path) -> Result<ArchiveFormat> {
    let format = ArchiveFormat::from_path(archive).ok_or_else(|| {
        ConvError::new(
            ErrorCode::UnknownFormat,
            format!("{} is not an archive conv knows", archive.display()),
        )
    })?;
    if !format.can_read() {
        return Err(ArchiveFormat::unreadable(archive));
    }
    if !archive.is_file() {
        return Err(input_not_found(archive));
    }
    Ok(format)
}

/// Puts `sources` into a new archive at `output`. A folder goes in as a
/// folder of its own name, a file under its own name. Finder's own files
/// (`.DS_Store`, `._` copies of extra file info, `__MACOSX`) are left out,
/// and a note says how many. Links are kept as links.
pub fn pack(
    sources: &[PathBuf],
    output: &Path,
    format: ArchiveFormat,
    opts: &PackOptions,
) -> Result<Report> {
    let sources: Vec<walk::Source> = sources.iter().cloned().map(walk::Source::Named).collect();
    write::pack(&sources, output, format, opts, Vec::new())
}

/// Opens `archive` at `dest`. See `Destination` for where it lands.
///
/// `confirm` is asked, before anything is written where it can be known
/// (zip and 7z list their sizes up front) or as soon as the unpacked size
/// passes the line (a compressed tar), whether to go on with an archive
/// that grows more than `ASK_ABOVE_RATIO` times past `ASK_ABOVE_BYTES`.
/// A no stops with `ErrorCode::ConfirmationRequired` and nothing kept.
pub fn unpack(
    archive: &Path,
    dest: &Destination,
    opts: &UnpackOptions,
    confirm: &mut dyn FnMut(&Expansion) -> bool,
) -> Result<Report> {
    let format = readable_format(archive)?;
    place::unpack(archive, format, dest, opts, confirm)
}

/// Rewrites `input` as a `format` archive at `output`, holding the same
/// entries under the same names. It is unpacked into a scratch folder
/// beside `output` first, so everything `unpack` refuses is refused here
/// too.
pub fn convert(
    input: &Path,
    output: &Path,
    format: ArchiveFormat,
    read_opts: &UnpackOptions,
    write_opts: &PackOptions,
    confirm: &mut dyn FnMut(&Expansion) -> bool,
) -> Result<Report> {
    let from = readable_format(input)?;
    if !format.can_write() {
        return Err(format.unwritable());
    }
    if output.exists() && !write_opts.overwrite {
        return Err(exists_error(output));
    }
    let dir = parent_dir(output);
    if !dir.is_dir() {
        return Err(failed(format!(
            "output directory does not exist: {}",
            dir.display()
        )));
    }
    let scratch = Scratch {
        path: dir.join(scratch_name("convert")),
        keep: false,
    };
    std::fs::create_dir(&scratch.path).map_err(|e| {
        failed(format!(
            "cannot make a scratch folder in {}: {e}",
            dir.display()
        ))
    })?;
    let opened = read::extract(
        input,
        from,
        &scratch.path,
        read_opts.password.as_deref(),
        confirm,
    )?;
    let mut notes = opened.notes;
    if opened.encrypted && write_opts.password.is_none() {
        notes.push(format!(
            "{} was password-protected; the new archive isn't. Add --password to lock it.",
            name_of(input)
        ));
    }
    let sources = [walk::Source::ContentsOf(scratch.path.clone())];
    write::pack(&sources, output, format, write_opts, notes)
}

/// Reads what `archive` holds without writing anything.
pub fn list(archive: &Path, password: Option<&str>) -> Result<Listing> {
    let format = readable_format(archive)?;
    read::list(archive, format, password)
}

/// The folder a path sits in, `.` for a bare name.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests;
