//! Writing an archive from what `walk` gathered.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use super::walk::{self, Item, ItemKind, Source};
use super::{
    exists_error, failed, parent_dir, scratch_name, time, ArchiveFormat, PackOptions, Report,
    Scratch,
};
use crate::{ConvError, ErrorCode, Result};

/// Already compressed: deflating these again costs time and saves nothing,
/// so a zip stores them as they are. Office and OpenDocument files are
/// zips themselves.
const STORED: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "mp4", "m4v", "mov", "mkv",
    "webm", "avi", "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "zip", "gz", "tgz", "xz",
    "txz", "bz2", "zst", "7z", "rar", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "jar",
    "apk", "ipa", "dmg", "pkg", "woff", "woff2",
];

fn is_compressed(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| STORED.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn pack(
    sources: &[Source],
    output: &Path,
    format: ArchiveFormat,
    opts: &PackOptions,
    mut notes: Vec<String>,
) -> Result<Report> {
    if !format.can_write() {
        return Err(format.unwritable());
    }
    if opts.password.is_some() && format != ArchiveFormat::Zip {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "--password locks zip archives only; a .{} can't hold one",
                format.ext()
            ),
        ));
    }
    if output.is_dir() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{} is a folder", output.display()),
        ));
    }
    if output.exists() && !opts.overwrite {
        return Err(exists_error(output));
    }
    let dir = parent_dir(output);
    if !dir.is_dir() {
        return Err(failed(format!(
            "output directory does not exist: {}",
            dir.display()
        )));
    }
    let temp_name = scratch_name("pack");
    let mut temp = Scratch {
        path: dir.join(&temp_name),
        keep: false,
    };
    let skip = walk::Skip {
        dir: std::fs::canonicalize(&dir).ok(),
        names: vec![
            output.file_name().unwrap_or_default().to_os_string(),
            temp_name.clone().into(),
        ],
    };
    let collected = walk::collect(sources, &skip)?;
    let file = File::create(&temp.path)
        .map_err(|e| failed(format!("cannot write {}: {e}", temp.path.display())))?;
    let wrote = |e: std::io::Error| failed(format!("cannot write {}: {e}", output.display()));
    match format {
        ArchiveFormat::Zip => write_zip(file, &collected.items, opts.password.as_deref())?,
        ArchiveFormat::Tar => {
            write_tar(BufWriter::new(file), &collected.items)?
                .flush()
                .map_err(wrote)?;
        }
        ArchiveFormat::TarGz => {
            let gz =
                flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::default());
            write_tar(gz, &collected.items)?
                .finish()
                .and_then(|mut w| w.flush())
                .map_err(wrote)?;
        }
        ArchiveFormat::TarXz => {
            let mut xz_opts = lzma_rust2::XzOptions::with_preset(6);
            // Three dictionaries a block, as `xz -T` does: each block is
            // compressed on its own thread.
            let block = u64::from(xz_opts.lzma_options.dict_size) * 3;
            xz_opts.set_block_size(std::num::NonZeroU64::new(block));
            let workers = std::thread::available_parallelism()
                .map(|n| n.get().min(8) as u32)
                .unwrap_or(1);
            let xz = lzma_rust2::XzWriterMt::new(BufWriter::new(file), xz_opts, workers)
                .map_err(wrote)?;
            write_tar(xz, &collected.items)?
                .finish()
                .and_then(|mut w| w.flush())
                .map_err(wrote)?;
        }
        ArchiveFormat::TarBz2 => {
            let bz = bzip2::write::BzEncoder::new(BufWriter::new(file), bzip2::Compression::best());
            write_tar(bz, &collected.items)?
                .finish()
                .and_then(|mut w| w.flush())
                .map_err(wrote)?;
        }
        ArchiveFormat::TarZst => {
            // Level 3, zstd's own default.
            let zst = zstd::stream::write::Encoder::new(BufWriter::new(file), 3).map_err(wrote)?;
            write_tar(zst, &collected.items)?
                .finish()
                .and_then(|mut w| w.flush())
                .map_err(wrote)?;
        }
        ArchiveFormat::SevenZ | ArchiveFormat::Rar => unreachable!("refused above"),
    }
    std::fs::rename(&temp.path, output).map_err(wrote)?;
    temp.keep = true;

    notes.extend(collected.notes());
    if opts.password.is_some() {
        notes.push(
            "Locked with AES-256. The file names stay readable; only what's in the files is \
             locked. macOS Archive Utility can't open it; 7-Zip, Keka, The Unarchiver and conv can."
                .to_string(),
        );
    }
    let count = |want: fn(&ItemKind) -> bool| -> u64 {
        collected.items.iter().filter(|i| want(&i.kind)).count() as u64
    };
    Ok(Report {
        output: output.to_path_buf(),
        format,
        files: count(|k| matches!(k, ItemKind::File { .. })),
        folders: count(|k| matches!(k, ItemKind::Dir)),
        links: count(|k| matches!(k, ItemKind::Symlink(_))),
        bytes: std::fs::metadata(output).map(|m| m.len()).unwrap_or(0),
        content_bytes: collected.content_bytes(),
        encrypted: opts.password.is_some(),
        notes,
    })
}

fn open_item(item: &Item) -> Result<File> {
    File::open(&item.path).map_err(|e| failed(format!("cannot read {}: {e}", item.path.display())))
}

fn write_zip(file: File, items: &[Item], password: Option<&str>) -> Result<()> {
    use zip::write::{ExtendedFileOptions, FileOptions};
    let zipped = |e: zip::result::ZipError| failed(format!("cannot write the zip: {e}"));
    let mut zip = zip::ZipWriter::new(BufWriter::new(file));
    for item in items {
        let mut opts: FileOptions<ExtendedFileOptions> =
            FileOptions::default().last_modified_time(time::to_dos(item.mtime));
        if let Some(mode) = item.mode {
            opts = opts.unix_permissions(mode & 0o777);
        }
        // The extended timestamp: the exact time in UTC, which the DOS one
        // above can only give to two seconds in some time zone.
        let secs = time::unix_secs(item.mtime).clamp(0, i64::from(u32::MAX)) as u32;
        let mut ut = vec![1u8];
        ut.extend_from_slice(&secs.to_le_bytes());
        opts.add_extra_field(0x5455, &ut, false).map_err(zipped)?;
        opts.add_extra_field(0x5455, &ut, true).map_err(zipped)?;
        match &item.kind {
            ItemKind::Dir => zip
                .add_directory(item.name.as_str(), opts)
                .map_err(zipped)?,
            ItemKind::Symlink(target) => zip
                .add_symlink(item.name.as_str(), target.to_string_lossy(), opts)
                .map_err(zipped)?,
            ItemKind::File { size } => {
                let method = if *size == 0 || is_compressed(&item.name) {
                    zip::CompressionMethod::Stored
                } else {
                    zip::CompressionMethod::Deflated
                };
                let mut opts = opts
                    .compression_method(method)
                    .large_file(*size >= u64::from(u32::MAX));
                if let Some(p) = password {
                    opts = opts.with_aes_encryption(zip::AesMode::Aes256, p);
                }
                zip.start_file(item.name.as_str(), opts).map_err(zipped)?;
                let mut src = open_item(item)?;
                std::io::copy(&mut src, &mut zip)
                    .map_err(|e| failed(format!("cannot pack {}: {e}", item.path.display())))?;
            }
        }
    }
    zip.finish()
        .map_err(zipped)?
        .flush()
        .map_err(|e| failed(format!("cannot write the zip: {e}")))
}

fn write_tar<W: Write>(w: W, items: &[Item]) -> Result<W> {
    let tarred = |e: std::io::Error| failed(format!("cannot write the tar: {e}"));
    let mut b = tar::Builder::new(w);
    for item in items {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(time::unix_secs(item.mtime).max(0) as u64);
        // Owner 0 with no names: what a tar for others to unpack should
        // carry, rather than this account's user and group.
        h.set_uid(0);
        h.set_gid(0);
        match &item.kind {
            ItemKind::Dir => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(item.mode.unwrap_or(0o755) & 0o7777);
                h.set_size(0);
                b.append_data(&mut h, format!("{}/", item.name), std::io::empty())
                    .map_err(tarred)?;
            }
            ItemKind::Symlink(target) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(item.mode.unwrap_or(0o777) & 0o7777);
                h.set_size(0);
                b.append_link(&mut h, &item.name, target).map_err(tarred)?;
            }
            ItemKind::File { .. } => {
                let src = open_item(item)?;
                // The size now, not when the folder was read, so the
                // header matches what is copied.
                let size = src.metadata().map(|m| m.len()).map_err(tarred)?;
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(item.mode.unwrap_or(0o644) & 0o7777);
                h.set_size(size);
                b.append_data(&mut h, &item.name, src.take(size))
                    .map_err(tarred)?;
            }
        }
    }
    b.into_inner().map_err(tarred)
}
