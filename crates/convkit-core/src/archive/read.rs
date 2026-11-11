//! Reading each archive format into a `Sink`.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use super::safe::{self, Entry, EntryKind, Extractor, Sink};
use super::{
    failed, name_of, password_needed, time, wrong_password, ArchiveFormat, Expansion, Listed,
    Listing,
};
use crate::tools::plural;
use crate::Result;

/// What reading found beyond the entries themselves.
#[derive(Debug, Default)]
struct ReadInfo {
    encrypted: bool,
    /// Zip names that were not UTF-8 and were read as DOS text.
    dos_names: u64,
}

/// What `extract` wrote.
#[derive(Debug, Default)]
pub(crate) struct Opened {
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    pub bytes: u64,
    pub encrypted: bool,
    pub notes: Vec<String>,
}

/// Writes every entry of `archive` under `root`, an empty folder.
pub(crate) fn extract(
    archive: &Path,
    format: ArchiveFormat,
    root: &Path,
    password: Option<&str>,
    confirm: &mut dyn FnMut(&Expansion) -> bool,
) -> Result<Opened> {
    let mut ex = Extractor::new(root, archive, confirm);
    let info = read(archive, format, password, &mut ex)?;
    let ex = ex.finish()?;
    let mut notes = Vec::new();
    if ex.mac_skipped > 0 {
        notes.push(
            "Left out __MACOSX, the Finder's copy of extra file info, which other apps show as clutter."
                .to_string(),
        );
    }
    if info.dos_names > 0 {
        notes.push(format!(
            "{} not marked as UTF-8 and read as old DOS text (CP437); check {} right.",
            plural(info.dos_names, "name was", "names were"),
            if info.dos_names == 1 {
                "it looks"
            } else {
                "they look"
            }
        ));
    }
    if ex.other_skipped > 0 {
        notes.push(format!(
            "Left out {} that {} a file, folder or link (a device or pipe).",
            plural(ex.other_skipped, "entry", "entries"),
            if ex.other_skipped == 1 {
                "isn't"
            } else {
                "aren't"
            }
        ));
    }
    if ex.windows_links_skipped > 0 {
        notes.push(format!(
            "Left out {}: Windows makes links only with Developer Mode on.",
            plural(ex.windows_links_skipped, "link", "links")
        ));
    }
    if let Some(mark) = safe::quarantine_of(archive) {
        safe::mark_downloaded(root, &mark);
        notes.push(
            "Marked as downloaded, as the archive is, so macOS checks any app in it before it opens."
                .to_string(),
        );
    }
    Ok(Opened {
        files: ex.files,
        folders: ex.folders,
        links: ex.link_count,
        bytes: ex.written(),
        encrypted: info.encrypted,
        notes,
    })
}

/// Names entries without writing them.
struct Lister {
    entries: Vec<Listed>,
    /// False for tar, whose reader skips what is not read.
    drain: bool,
}

impl Sink for Lister {
    fn wants_data(&self) -> bool {
        false
    }
    fn entry(&mut self, entry: Entry, data: &mut dyn Read) -> super::Result<()> {
        let kind = match entry.kind {
            EntryKind::File | EntryKind::Hardlink(_) => "file",
            EntryKind::Dir => "folder",
            EntryKind::Symlink(_) => "link",
            EntryKind::Other => "other",
        };
        // The size the header gives is what is counted.
        if self.drain {
            let _ = std::io::copy(data, &mut std::io::sink());
        }
        self.entries.push(Listed {
            name: entry.display(),
            kind,
            size: entry.size,
        });
        Ok(())
    }
}

pub(crate) fn list(
    archive: &Path,
    format: ArchiveFormat,
    password: Option<&str>,
) -> Result<Listing> {
    let mut lister = Lister {
        entries: Vec::new(),
        drain: matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ),
    };
    let info = read(archive, format, password, &mut lister)?;
    Ok(Listing {
        format,
        entries: lister.entries,
        encrypted: info.encrypted,
    })
}

fn open(archive: &Path) -> Result<File> {
    File::open(archive).map_err(|e| failed(format!("cannot open {}: {e}", archive.display())))
}

fn read(
    archive: &Path,
    format: ArchiveFormat,
    password: Option<&str>,
    sink: &mut dyn Sink,
) -> Result<ReadInfo> {
    match format {
        ArchiveFormat::Zip => read_zip(archive, password, sink),
        ArchiveFormat::SevenZ => read_7z(archive, password, sink),
        ArchiveFormat::Rar => Err(ArchiveFormat::unreadable(archive)),
        _ => read_tar(archive, format, sink),
    }
}

fn damaged(archive: &Path, what: &str, e: impl std::fmt::Display) -> crate::ConvError {
    failed(format!("cannot read {} as {what}: {e}", name_of(archive)))
}

fn read_zip(archive: &Path, password: Option<&str>, sink: &mut dyn Sink) -> Result<ReadInfo> {
    let mut zip = zip::ZipArchive::new(BufReader::new(open(archive)?))
        .map_err(|e| damaged(archive, "a zip", e))?;
    let mut info = ReadInfo::default();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let f = zip
            .by_index_raw(i)
            .map_err(|e| damaged(archive, "a zip", e))?;
        total = total.saturating_add(f.size());
        info.encrypted |= f.encrypted();
    }
    let data = sink.wants_data();
    if info.encrypted && password.is_none() && data {
        return Err(password_needed(archive));
    }
    sink.listed(total)?;
    for i in 0..zip.len() {
        let encrypted = zip
            .by_index_raw(i)
            .map_err(|e| damaged(archive, "a zip", e))?
            .encrypted();
        let opened = match password {
            _ if !data => zip.by_index_raw(i),
            Some(p) if encrypted => zip.by_index_decrypt(i, p.as_bytes()),
            _ => zip.by_index(i),
        };
        let mut f = match opened {
            Ok(f) => f,
            Err(zip::result::ZipError::InvalidPassword) => return Err(wrong_password(archive)),
            Err(e) => return Err(damaged(archive, "a zip", e)),
        };
        // macOS and most tools write UTF-8 names without setting the flag
        // that says so; a name that is valid UTF-8 is taken as UTF-8, and
        // only one that isn't falls back to the zip crate's DOS reading.
        let name = match std::str::from_utf8(f.name_raw()) {
            Ok(s) => s.to_string(),
            Err(_) => {
                info.dos_names += 1;
                f.name()
                    .map_err(|e| damaged(archive, "a zip", e))?
                    .into_owned()
            }
        };
        // Zips made on Windows sometimes separate folders with `\`.
        let name = PathBuf::from(name.replace('\\', "/"));
        let mode = f.unix_mode();
        let mtime = f
            .extra_data_fields()
            .find_map(|x| match x {
                zip::ExtraField::ExtendedTimestamp(t) => t.mod_time(),
                _ => None,
            })
            .and_then(|s| time::from_unix_secs(i64::from(s)))
            .or_else(|| f.last_modified().and_then(time::from_dos));
        let size = f.size();
        let kind = if f.is_dir() {
            EntryKind::Dir
        } else if f.is_symlink() && !data {
            EntryKind::Symlink(PathBuf::new())
        } else if f.is_symlink() {
            let mut target = Vec::new();
            (&mut f)
                .take(4096)
                .read_to_end(&mut target)
                .map_err(|e| safe::read_error(archive, &name.to_string_lossy(), &e))?;
            EntryKind::Symlink(bytes_to_path(target))
        } else {
            EntryKind::File
        };
        sink.entry(
            Entry {
                name,
                kind,
                mode,
                mtime,
                size,
            },
            &mut f,
        )?;
    }
    Ok(info)
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).replace('/', "\\"))
}

/// The decompressed tar stream of a `.tar*` archive.
fn tar_stream(archive: &Path, format: ArchiveFormat) -> Result<Box<dyn Read>> {
    let file = BufReader::with_capacity(256 * 1024, open(archive)?);
    Ok(match format {
        ArchiveFormat::Tar => Box::new(file),
        ArchiveFormat::TarGz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        ArchiveFormat::TarXz => Box::new(lzma_rust2::XzReader::new(file, true)),
        ArchiveFormat::TarBz2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        ArchiveFormat::TarZst => Box::new(
            zstd::stream::read::Decoder::with_buffer(file)
                .map_err(|e| damaged(archive, "zstd", e))?,
        ),
        _ => unreachable!("tar_stream is only called for tar formats"),
    })
}

fn read_tar(archive: &Path, format: ArchiveFormat, sink: &mut dyn Sink) -> Result<ReadInfo> {
    let what = format!("a .{} archive", format.ext());
    let mut tar = tar::Archive::new(tar_stream(archive, format)?);
    let entries = tar.entries().map_err(|e| damaged(archive, &what, e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| damaged(archive, &what, e))?;
        let name = entry
            .path()
            .map_err(|e| damaged(archive, &what, e))?
            .into_owned();
        let header = entry.header();
        let mode = header.mode().ok();
        let mtime = header
            .mtime()
            .ok()
            .and_then(|s| time::from_unix_secs(i64::try_from(s).unwrap_or(i64::MAX)));
        let size = header.size().unwrap_or(0);
        let link = || -> Result<PathBuf> {
            Ok(entry
                .link_name()
                .map_err(|e| damaged(archive, &what, e))?
                .map(|l| l.into_owned())
                .unwrap_or_default())
        };
        let kind = match header.entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                EntryKind::File
            }
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Symlink => EntryKind::Symlink(link()?),
            tar::EntryType::Link => EntryKind::Hardlink(link()?),
            // Extension headers the tar crate has already applied.
            tar::EntryType::XGlobalHeader
            | tar::EntryType::XHeader
            | tar::EntryType::GNULongName
            | tar::EntryType::GNULongLink => continue,
            _ => EntryKind::Other,
        };
        sink.entry(
            Entry {
                name,
                kind,
                mode,
                mtime,
                size,
            },
            &mut entry,
        )?;
    }
    Ok(ReadInfo::default())
}

fn read_7z(archive: &Path, password: Option<&str>, sink: &mut dyn Sink) -> Result<ReadInfo> {
    use sevenz_rust2::{ArchiveReader, Error, Password};
    let pw = password.map_or_else(Password::empty, Password::new);
    let mut reader = match ArchiveReader::new(BufReader::new(open(archive)?), pw) {
        Ok(r) => r,
        Err(Error::PasswordRequired) if password.is_none() => return Err(password_needed(archive)),
        Err(Error::PasswordRequired | Error::MaybeBadPassword(_)) => {
            return Err(wrong_password(archive))
        }
        Err(e) => return Err(damaged(archive, "a 7z", e)),
    };
    let total: u64 = reader
        .archive()
        .files
        .iter()
        .map(|f| f.size)
        .fold(0, u64::saturating_add);
    let encrypted = reader.archive().blocks.iter().any(|b| {
        b.coders
            .iter()
            .any(|c| c.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256)
    });
    sink.listed(total)?;
    // The reader's callback returns its own error type, so a refusal from
    // the sink is kept here and the walk stopped.
    let mut stopped: Option<crate::ConvError> = None;
    let walked = reader.for_each_entries(|e, data| {
        if e.is_anti_item {
            return Ok(true);
        }
        let name = PathBuf::from(e.name.replace('\\', "/"));
        // 7-Zip keeps a Unix mode in the attribute's top 16 bits, flagged
        // with 0x8000.
        let mode = (e.has_windows_attributes && e.windows_attributes & 0x8000 != 0)
            .then_some(e.windows_attributes >> 16);
        let is_link = mode.is_some_and(|m| m & 0o170000 == 0o120000);
        let mtime = e
            .has_last_modified_date
            .then(|| std::time::SystemTime::from(e.last_modified_date));
        let kind = if e.is_directory {
            EntryKind::Dir
        } else if is_link {
            let mut target = Vec::new();
            data.take(4096).read_to_end(&mut target)?;
            EntryKind::Symlink(bytes_to_path(target))
        } else {
            EntryKind::File
        };
        let entry = Entry {
            name,
            kind,
            mode,
            mtime,
            size: e.size,
        };
        match sink.entry(entry, data) {
            Ok(()) => Ok(true),
            Err(err) => {
                stopped = Some(err);
                Ok(false)
            }
        }
    });
    if let Some(e) = stopped {
        return Err(e);
    }
    match walked {
        Ok(()) => Ok(ReadInfo {
            encrypted,
            dos_names: 0,
        }),
        Err(Error::PasswordRequired) if password.is_none() => Err(password_needed(archive)),
        Err(Error::PasswordRequired | Error::MaybeBadPassword(_)) => Err(wrong_password(archive)),
        Err(e) => Err(damaged(archive, "a 7z", e)),
    }
}
