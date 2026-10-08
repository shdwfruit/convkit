//! Downloads, verifies, and unpacks a managed backend's binary.
//!
//! This module never prints anything — like the rest of `convkit-core`, all
//! progress reporting belongs to the `conv` binary. It also never touches a
//! backend it isn't handed a verified [`manifest::Asset`] for; deciding
//! *which* asset to use (or refusing when none exists) is the caller's job.

use std::ffi::OsStr;
use std::io::{Cursor, Read};
use std::path::Component;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::backend::ManagedLayout;
use crate::error::{ConvError, ErrorCode, Remediation, Result};
use crate::manifest::{ArchiveMember, Asset, Packaging};
use crate::Backend;

/// Upper bound on any single read this module performs — the HTTP response
/// body, and each archive member pulled out of it. `ureq` decodes a gzipped
/// response transparently, and a zip/tar member's declared uncompressed
/// size can lie, so without a cap a malicious or merely broken endpoint
/// could inflate a small response into an unbounded `Vec` long before
/// `verify` ever gets a chance to reject it. 512 MiB comfortably covers the
/// largest real asset in the manifest (the Windows ffmpeg zip, ~110 MiB)
/// with a lot of headroom.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Hashes `bytes` and compares the lowercase hex digest against
/// `expected_sha256`, byte-by-byte without an early exit, so this doesn't
/// leak comparison length via timing beyond the (public, fixed) digest
/// length itself. On mismatch, the error message contains the word
/// "checksum" so a caller — or a human reading `--json` output — can tell
/// this apart from a network or extraction failure.
pub fn verify(bytes: &[u8], expected_sha256: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();

    let mut actual = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(actual, "{b:02x}");
    }

    let expected = expected_sha256.to_ascii_lowercase();
    let matches = actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;

    if matches {
        Ok(())
    } else {
        Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!("checksum mismatch: expected {expected}, got {actual}"),
        ))
    }
}

fn io_err(path: &Path, e: std::io::Error) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("{}: {e}", path.display()),
    )
}

fn download_err(url: &str, e: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("failed to download {url}: {e}"),
    )
}

fn extract_err(asset: &Asset, member: &ArchiveMember, e: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!(
            "failed to extract {} from {}: {e}",
            member.backend.exe_name(),
            asset.url
        ),
    )
}

/// Reads all of `r`, refusing anything past `MAX_DOWNLOAD_BYTES`. Reads one
/// byte beyond the cap (`take(MAX_DOWNLOAD_BYTES + 1)`) specifically so a
/// response of exactly the cap size is distinguishable from one that
/// overflows it, rather than a merely-at-the-limit response being silently
/// (and wrongly) treated as too large.
fn read_capped(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.by_ref()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(std::io::Error::other(format!(
            "exceeds the {MAX_DOWNLOAD_BYTES}-byte cap"
        )));
    }
    Ok(buf)
}

/// GETs `url` into memory, capped at `MAX_DOWNLOAD_BYTES`. No redirects to
/// worry about beyond what `ureq` follows by default — GitHub release-asset
/// URLs redirect once, to S3 or Azure blob storage, which `ureq` follows
/// transparently.
fn download(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(300))
        .call()
        .map_err(|e| download_err(url, e))?;
    read_capped(resp.into_reader()).map_err(|e| download_err(url, e))
}

fn extract_zip(asset: &Asset, member: &ArchiveMember, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| extract_err(asset, member, e))?;
    let file = archive.by_name(member.archive_member).map_err(|e| {
        extract_err(
            asset,
            member,
            format!("{} not found: {e}", member.archive_member),
        )
    })?;
    read_capped(file).map_err(|e| extract_err(asset, member, e))
}

/// A tar entry's path as stored may carry a leading `./` (many tar tools
/// write GNU-format entries this way); the manifest's `archive_member`
/// values never do, so normalise both sides the same way rather than
/// requiring the manifest to guess the exact byte-for-byte spelling a given
/// tarball uses.
fn strip_leading_dot_slash(s: &str) -> &str {
    s.strip_prefix("./").unwrap_or(s)
}

/// Walks every entry of an already-opened tar `archive`, returning the bytes
/// of the one entry whose (dot-slash-normalised) name matches
/// `member.archive_member` — capped the same as every other read in this
/// module. Shared by `extract_tar_gz` and `extract_tar_xz`, which differ only
/// in how they get from compressed bytes to a `Read` of the raw tar stream;
/// this is where the archive-supplied name is compared, and it is compared
/// only, never joined onto a filesystem path — the entry's *bytes* are what
/// this returns, not a path anyone writes to.
fn extract_tar_member<R: Read>(
    asset: &Asset,
    member: &ArchiveMember,
    mut archive: tar::Archive<R>,
) -> Result<Vec<u8>> {
    let entries = archive
        .entries()
        .map_err(|e| extract_err(asset, member, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| extract_err(asset, member, e))?;
        let path = entry.path().map_err(|e| extract_err(asset, member, e))?;
        let name = path.to_string_lossy();
        if strip_leading_dot_slash(&name) == member.archive_member {
            return read_capped(entry).map_err(|e| extract_err(asset, member, e));
        }
    }
    Err(extract_err(
        asset,
        member,
        format!("{} not found in archive", member.archive_member),
    ))
}

fn extract_tar_gz(asset: &Asset, member: &ArchiveMember, bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    extract_tar_member(asset, member, tar::Archive::new(decoder))
}

/// A `Write` sink capped at `cap` bytes, so decompressing a small `.xz`
/// stream that expands into something enormous (accidentally, or a hostile
/// endpoint) fails the moment it would exceed the cap rather than growing an
/// unbounded `Vec` first. `lzma-rs` writes straight to whatever `Write` it's
/// given, so this is the xz equivalent of `read_capped`'s `take(cap + 1)`
/// trick for the other readers in this module.
struct CappedWriter {
    buf: Vec<u8>,
    cap: u64,
}

impl CappedWriter {
    fn new(cap: u64) -> Self {
        CappedWriter {
            buf: Vec::new(),
            cap,
        }
    }
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let would_be = self.buf.len() as u64 + data.len() as u64;
        if would_be > self.cap {
            return Err(std::io::Error::other(format!(
                "exceeds the {} byte cap",
                self.cap
            )));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decodes a whole `.xz` stream into memory via `lzma-rs` — a pure-Rust
/// decoder (`#![forbid(unsafe_code)]`, no C dependency), chosen specifically
/// so this doesn't need `xz2`/`liblzma-sys`, which link a C library and would
/// make the Windows build depend on a C toolchain the same way `.7z` support
/// would. Output is capped at `MAX_DOWNLOAD_BYTES`, same as every other read
/// in this module.
fn decompress_xz_capped(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut input = Cursor::new(bytes);
    let mut output = CappedWriter::new(MAX_DOWNLOAD_BYTES);
    lzma_rs::xz_decompress(&mut input, &mut output).map_err(std::io::Error::other)?;
    Ok(output.buf)
}

fn extract_tar_xz(asset: &Asset, member: &ArchiveMember, bytes: &[u8]) -> Result<Vec<u8>> {
    let tar_bytes = decompress_xz_capped(bytes).map_err(|e| extract_err(asset, member, e))?;
    extract_tar_member(asset, member, tar::Archive::new(Cursor::new(tar_bytes)))
}

/// Extracts the bytes for one [`ArchiveMember`] of `asset` out of the
/// already-downloaded, already-verified `bytes`. Called once per member in
/// `asset.members` by `fetch_and_install`, all against the same in-memory
/// `bytes` — the whole point of naming several members on one `Asset` is
/// that this never re-downloads for the second (or third) one.
fn extract(asset: &Asset, member: &ArchiveMember, bytes: &[u8]) -> Result<Vec<u8>> {
    match asset.packaging {
        Packaging::Raw => Ok(bytes.to_vec()),
        Packaging::Zip => extract_zip(asset, member, bytes),
        Packaging::TarGz => extract_tar_gz(asset, member, bytes),
        Packaging::TarXz => extract_tar_xz(asset, member, bytes),
    }
}

/// Ad-hoc-signs the binary at `path` via `codesign --force --sign - <path>`.
/// Only ever called on macOS arm64. An unsigned arm64 binary is killed by
/// the kernel with an undiagnosable `Killed: 9` on first launch, so this
/// fails loudly — rather than leaving a binary that looks installed but
/// cannot run — when `codesign` itself can't be found or exits non-zero.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn codesign(path: &Path) -> Result<()> {
    let outcome = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(path)
        .status();
    match outcome {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "codesign exited with {status} while signing {}; \
                 an unsigned arm64 binary will be killed on launch",
                path.display()
            ),
        )),
        Err(e) => Err(ConvError::new(
            ErrorCode::ConversionFailed,
            format!(
                "codesign is required to run a downloaded binary on Apple \
                 Silicon but could not be run: {e}"
            ),
        )),
    }
}

/// Sets the destination file's mode and, on macOS arm64, ad-hoc-signs it.
fn finalize(dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_err(dest, e))?;
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        codesign(dest)?;
    }
    #[cfg(not(unix))]
    {
        let _ = dest;
    }
    Ok(())
}

/// The sibling path `fetch_and_install` writes and finalizes into before
/// renaming over `dest` — same directory, same filename plus a `.part`
/// suffix, so the rename is a same-filesystem, same-directory rename and
/// therefore atomic on every platform this crate targets.
fn temp_path_for(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Writes `bytes` to `tmp` and finalizes it in place (permissions, and on
/// macOS arm64, an ad-hoc signature) — everything that can fail, before
/// `dest` is touched at all.
fn write_and_finalize(tmp: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(tmp, bytes).map_err(|e| io_err(tmp, e))?;
    finalize(tmp)
}

/// Writes `bytes` to a `.part` sibling of `dest`, finalizes it there, and
/// only then renames it into place. Split out from `fetch_and_install` so
/// this atomic-install sequence — the part review finding 1 was about — can
/// be exercised directly by a test, without a network fetch.
///
/// Never leaves a partial or unsigned file at `dest` itself: if the write,
/// `chmod`, `codesign`, or the rename fails, `dest` is never created or
/// modified, and the `.part` file is removed on a best-effort basis.
/// Without this, a `codesign` failure on macOS arm64 would previously leave
/// an unsigned binary sitting at `dest`, which `Resolver::resolve` would
/// then treat as a successful install (it only checks `is_file()`) and
/// permanently shadow a working `PATH` install with a binary that dies with
/// `Killed: 9` on first launch — precisely the failure mode `codesign`
/// exists to prevent.
fn install_bytes(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path_for(dest);
    if let Err(e) = write_and_finalize(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(dest, e));
    }
    Ok(())
}

/// One file a folder install writes, relative to the backend's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FolderEntry {
    File { rel: PathBuf, bytes: Vec<u8> },
    Symlink { rel: PathBuf, target: String },
}

/// The program's path inside its folder, `bin/<file name>`, taken from the
/// member's `archive_member` (`qpdf-12.4.2-msvc64/bin/qpdf.exe` gives
/// `bin/qpdf.exe`).
fn folder_exe_rel(member: &ArchiveMember) -> String {
    let file = member.archive_member.rsplit('/').next().unwrap_or("");
    format!("bin/{file}")
}

/// A single path component with nothing that could climb out of a
/// directory: no separators of either kind, and not `.` or `..`.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

/// Whether `rel`, a `/`-separated path inside the folder, is something the
/// program needs at run time: the program itself, the DLLs beside it on
/// Windows, or a shared library directly in `lib/` elsewhere. Headers,
/// docs, static and import libraries, CMake and pkg-config files, and
/// upstream's other tools are left out.
fn keep_in_folder(rel: &str, exe_rel: &str) -> bool {
    if rel == exe_rel {
        return true;
    }
    let lower = rel.to_ascii_lowercase();
    if let Some(name) = lower.strip_prefix("bin/") {
        return plain_file_name(name) && name.ends_with(".dll");
    }
    if let Some(name) = lower.strip_prefix("lib/") {
        return plain_file_name(name)
            && (name.ends_with(".dylib") || name.ends_with(".so") || name.contains(".so."));
    }
    false
}

/// Whether a symlink at `link` (relative to the folder) pointing at
/// `target` resolves to something inside the folder. Only relative targets
/// qualify.
fn link_stays_inside(link: &Path, target: &str) -> bool {
    let target = Path::new(target);
    if target.is_absolute() || target.has_root() {
        return false;
    }
    let mut parts: Vec<&OsStr> = link
        .parent()
        .map(|p| p.iter().collect())
        .unwrap_or_default();
    for c in target.components() {
        match c {
            Component::Normal(n) => parts.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    !parts.is_empty()
}

/// Reads a folder backend's runtime files out of a verified zip: every
/// entry under the member's root (`qpdf-12.4.2-msvc64/` on Windows, the
/// archive root elsewhere) that `keep_in_folder` keeps, with paths made
/// relative to that root. Everything is read before anything is written,
/// so a bad archive fails without touching the disk.
fn extract_folder(asset: &Asset, member: &ArchiveMember, bytes: &[u8]) -> Result<Vec<FolderEntry>> {
    if asset.packaging != Packaging::Zip {
        return Err(extract_err(
            asset,
            member,
            "a folder install needs a zip archive",
        ));
    }
    let exe_rel = folder_exe_rel(member);
    let root = member
        .archive_member
        .strip_suffix(exe_rel.as_str())
        .ok_or_else(|| {
            extract_err(
                asset,
                member,
                format!("{} does not end in {exe_rel}", member.archive_member),
            )
        })?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| extract_err(asset, member, e))?;
    let mut entries = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| extract_err(asset, member, e))?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let Some(rel) = name.strip_prefix(root) else {
            continue;
        };
        if !keep_in_folder(rel, &exe_rel) {
            continue;
        }
        let rel_path = PathBuf::from(rel);
        let data = read_capped(&mut file).map_err(|e| extract_err(asset, member, e))?;
        if file.is_symlink() {
            let target = String::from_utf8(data).map_err(|e| extract_err(asset, member, e))?;
            if !link_stays_inside(&rel_path, &target) {
                return Err(extract_err(
                    asset,
                    member,
                    format!("{rel} links outside its folder, to {target}"),
                ));
            }
            entries.push(FolderEntry::Symlink {
                rel: rel_path,
                target,
            });
        } else {
            entries.push(FolderEntry::File {
                rel: rel_path,
                bytes: data,
            });
        }
    }
    let has_program = entries
        .iter()
        .any(|e| matches!(e, FolderEntry::File { rel, .. } if rel == Path::new(&exe_rel)));
    if !has_program {
        return Err(extract_err(
            asset,
            member,
            format!("{} not found in archive", member.archive_member),
        ));
    }
    Ok(entries)
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|e| io_err(link, e))
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, link: &Path) -> Result<()> {
    Err(ConvError::new(
        ErrorCode::ConversionFailed,
        format!(
            "{}: the archive has a symlink, which installs on this platform do not support",
            link.display()
        ),
    ))
}

/// Writes `entries` under `dir`. The program and any `.dylib` go through
/// `finalize` (mode 0755, and an ad-hoc signature on macOS arm64, where an
/// unsigned library is refused at load time just as an unsigned program is
/// killed at launch).
fn write_folder(dir: &Path, exe_rel: &str, entries: &[FolderEntry]) -> Result<()> {
    for entry in entries {
        let (rel, path) = match entry {
            FolderEntry::File { rel, .. } | FolderEntry::Symlink { rel, .. } => {
                (rel, dir.join(rel))
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        match entry {
            FolderEntry::File { bytes, .. } => {
                std::fs::write(&path, bytes).map_err(|e| io_err(&path, e))?;
                let is_dylib = rel.extension().is_some_and(|e| e == "dylib");
                if rel == Path::new(exe_rel) || is_dylib {
                    finalize(&path)?;
                }
            }
            FolderEntry::Symlink { target, .. } => make_symlink(target, &path)?,
        }
    }
    Ok(())
}

fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Moves the finished temp folder to `folder`. An existing folder is moved
/// aside first and put back if the second move fails, so a failed swap
/// leaves the previous install working.
fn swap_into_place(tmp: &Path, folder: &Path) -> Result<()> {
    if folder.symlink_metadata().is_err() {
        return std::fs::rename(tmp, folder).map_err(|e| {
            remove_any(tmp);
            io_err(folder, e)
        });
    }
    let name = folder.file_name().unwrap_or_default().to_string_lossy();
    let old = folder.with_file_name(format!(".{name}.old-{}", std::process::id()));
    remove_any(&old);
    if let Err(e) = std::fs::rename(folder, &old) {
        remove_any(tmp);
        return Err(io_err(folder, e));
    }
    if let Err(e) = std::fs::rename(tmp, folder) {
        let _ = std::fs::rename(&old, folder);
        remove_any(tmp);
        return Err(io_err(folder, e));
    }
    remove_any(&old);
    Ok(())
}

/// Installs a folder backend. `dest_exe` is `Resolver::managed_path`
/// (`<managed dir>/<exe>/bin/<exe>`), so the folder is two levels up.
/// Writes into `.<exe>.part-<pid>` beside the folder, runs `check` on the
/// program there, and only then swaps it in. Any failure removes the temp
/// folder and leaves an existing install untouched.
fn install_folder(
    dest_exe: &Path,
    exe_rel: &str,
    entries: &[FolderEntry],
    check: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let folder = dest_exe.parent().and_then(Path::parent).ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("no folder above {}", dest_exe.display()),
        )
    })?;
    let parent = folder
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    let name = folder.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.part-{}", std::process::id()));
    remove_any(&tmp);
    let prepared = write_folder(&tmp, exe_rel, entries).and_then(|()| check(&tmp.join(exe_rel)));
    if let Err(e) = prepared {
        remove_any(&tmp);
        return Err(e);
    }
    swap_into_place(&tmp, folder)
}

/// The run check for a freshly unpacked folder backend: its `--version`
/// must answer. On Linux the usual reason it does not is a glibc older than
/// the one upstream built against; qpdf 12.4.2's Linux builds need 2.34.
fn check_runs(backend: Backend, exe: &Path) -> Result<()> {
    if crate::resolve::Resolver::probe_version(backend, exe).is_some() {
        return Ok(());
    }
    let mut message = format!(
        "the downloaded {} did not run on this system",
        backend.exe_name()
    );
    if cfg!(target_os = "linux") && backend == Backend::Qpdf {
        message.push_str(
            "; the prebuilt qpdf needs glibc 2.34 or newer \
             (Ubuntu 22.04, Debian 12, Fedora 35, RHEL 9)",
        );
    }
    Err(ConvError {
        code: ErrorCode::ConversionFailed,
        message,
        backend: Some(backend),
        remediation: Some(Remediation {
            managed: None,
            manual: Some(crate::error::manual_hint_for(backend)),
        }),
    })
}

/// Downloads `asset`, verifies its checksum exactly once, then extracts and
/// installs *every* member `asset.members` names — not just the one whose
/// backend the caller originally asked to install. `dest_for` maps each
/// member's backend to its final path (in production, always
/// `Resolver::managed_path`); this module stays decoupled from `Resolver`
/// itself, taking a plain callback instead.
///
/// This is the entire mechanism behind "one download provisions every
/// binary it contains": an asset whose upstream release bundles several
/// tools (today: ffmpeg + ffprobe on Windows) is downloaded and its checksum
/// verified once, and each member is extracted from those same in-memory
/// bytes — `download`/`verify` are called exactly once per call to this
/// function, regardless of how many members `asset.members` names.
///
/// Every member is extracted before any of them is written to disk, so a
/// corrupt or truncated archive that's missing one named member fails
/// before touching the filesystem at all, rather than leaving the pair
/// half-installed. Each member that *is* written still goes through
/// `install_bytes`'s atomic temp-name/finalize/rename sequence individually
/// — see its docs for the cleanup guarantee that provides per file.
///
/// A `ManagedLayout::Folder` member installs as a folder; see `install_folder`.
///
/// Returns `(backend, path)` for every member actually installed, on
/// success.
pub fn fetch_and_install(
    asset: &Asset,
    dest_for: impl Fn(Backend) -> PathBuf,
) -> Result<Vec<(Backend, PathBuf)>> {
    let bytes = download(asset.url)?;
    verify(&bytes, asset.sha256)?;
    install_verified(asset, &bytes, dest_for, &check_runs)
}

/// Everything `fetch_and_install` does once the download is verified,
/// split out so tests can drive it with in-memory archives and a stand-in
/// for the run check. Every member is extracted before any is written.
fn install_verified(
    asset: &Asset,
    bytes: &[u8],
    dest_for: impl Fn(Backend) -> PathBuf,
    check: &dyn Fn(Backend, &Path) -> Result<()>,
) -> Result<Vec<(Backend, PathBuf)>> {
    enum Extracted {
        File(Vec<u8>),
        Folder(Vec<FolderEntry>),
    }
    let mut extracted = Vec::with_capacity(asset.members.len());
    for member in asset.members {
        let e = match member.backend.managed_layout() {
            ManagedLayout::File => Extracted::File(extract(asset, member, bytes)?),
            ManagedLayout::Folder => Extracted::Folder(extract_folder(asset, member, bytes)?),
        };
        extracted.push((member, e));
    }

    let mut installed = Vec::with_capacity(extracted.len());
    for (member, e) in extracted {
        let dest = dest_for(member.backend);
        match e {
            Extracted::File(exe_bytes) => {
                let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
                std::fs::create_dir_all(dest_dir).map_err(|e| io_err(dest_dir, e))?;
                install_bytes(&dest, &exe_bytes)?;
            }
            Extracted::Folder(entries) => {
                let backend = member.backend;
                install_folder(&dest, &folder_exe_rel(member), &entries, &|exe| {
                    check(backend, exe)
                })?;
            }
        }
        installed.push((member.backend, dest));
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_rejects_a_hash_mismatch() {
        let e = verify(b"hello", &"0".repeat(64)).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert!(e.message.contains("checksum"), "{}", e.message);
    }

    #[test]
    fn verify_accepts_the_real_digest() {
        // sha256("hello"), computed with `printf 'hello' | sha256sum`.
        let d = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify(b"hello", d).is_ok());
    }

    #[test]
    fn verify_is_case_insensitive_on_the_expected_digest() {
        let d = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify(b"hello", d).is_ok());
    }

    #[test]
    fn verify_rejects_wrong_length_input_rather_than_panicking() {
        let e = verify(b"hello", "abcd").unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
    }

    /// A single-member `ArchiveMember` for `backend`.
    fn one_member(backend: crate::Backend, archive_member: &'static str) -> ArchiveMember {
        ArchiveMember {
            backend,
            archive_member,
        }
    }

    /// `Asset::members` is `&'static [ArchiveMember]` — the real manifest
    /// only ever builds these as `static`s, but a test asset is a short-lived
    /// local value, so this leaks it into a `'static` slice rather than
    /// changing the production field's lifetime. Harmless: it only happens
    /// inside a test process that exits shortly after.
    fn leaked(ms: Vec<ArchiveMember>) -> &'static [ArchiveMember] {
        Box::leak(ms.into_boxed_slice())
    }

    #[test]
    fn extract_raw_returns_the_bytes_unchanged() {
        let member = one_member(crate::Backend::Ffmpeg, "");
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/ffmpeg",
            sha256: "0",
            packaging: Packaging::Raw,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, b"not-really-an-executable").unwrap();
        assert_eq!(out, b"not-really-an-executable");
    }

    /// Builds a tiny in-memory zip with one member, so `extract_zip` can be
    /// exercised without a network fetch.
    fn make_test_zip(member: &str, contents: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            writer.start_file(member, options).unwrap();
            std::io::Write::write_all(&mut writer, contents).unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extract_zip_finds_the_named_member() {
        let bytes = make_test_zip("bin/tool.exe", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Ffmpeg, "bin/tool.exe");
        let asset = Asset {
            os: "windows",
            arch: "x64",
            url: "https://example.invalid/tool.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    #[test]
    fn extract_zip_reports_a_missing_member() {
        let bytes = make_test_zip("bin/tool.exe", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Ffmpeg, "bin/other.exe");
        let asset = Asset {
            os: "windows",
            arch: "x64",
            url: "https://example.invalid/tool.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            members: leaked(vec![member]),
            version: "0",
        };
        let e = extract(&asset, &member, &bytes).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
    }

    /// The mechanism this task adds: one downloaded zip with two named
    /// members must yield both binaries' bytes from the same in-memory
    /// download — `extract` is called once per member, but the caller
    /// (`fetch_and_install`) only ever downloads once.
    #[test]
    fn extract_pulls_every_member_out_of_one_shared_zip_download() {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            writer.start_file("bin/ffmpeg.exe", options).unwrap();
            std::io::Write::write_all(&mut writer, b"pretend-ffmpeg").unwrap();
            writer.start_file("bin/ffprobe.exe", options).unwrap();
            std::io::Write::write_all(&mut writer, b"pretend-ffprobe").unwrap();
            writer.finish().unwrap();
        }
        let ffmpeg_member = one_member(crate::Backend::Ffmpeg, "bin/ffmpeg.exe");
        let ffprobe_member = one_member(crate::Backend::Ffprobe, "bin/ffprobe.exe");
        let asset = Asset {
            os: "windows",
            arch: "x64",
            url: "https://example.invalid/bundle.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            members: leaked(vec![ffmpeg_member, ffprobe_member]),
            version: "0",
        };
        assert_eq!(
            extract(&asset, &ffmpeg_member, &buf).unwrap(),
            b"pretend-ffmpeg"
        );
        assert_eq!(
            extract(&asset, &ffprobe_member, &buf).unwrap(),
            b"pretend-ffprobe"
        );
    }

    /// Builds a tiny in-memory .tar.gz with one member, so `extract_tar_gz`
    /// can be exercised without a network fetch.
    fn make_test_tar_gz(member: &str, contents: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, member, Cursor::new(contents))
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    #[test]
    fn extract_tar_gz_finds_the_named_member() {
        let bytes = make_test_tar_gz("pkg/bin/tool", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Pandoc, "pkg/bin/tool");
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.gz",
            sha256: "0",
            packaging: Packaging::TarGz,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    /// Review finding 3: a real tarball can store its entry name with a
    /// leading `./` (GNU tar commonly does when archiving `.` recursively).
    /// The manifest's `archive_member` values never carry that prefix, so
    /// extraction must normalise it away rather than fail to find a member
    /// that is, in fact, present.
    #[test]
    fn extract_tar_gz_tolerates_a_leading_dot_slash_in_the_entry_name() {
        let bytes = make_test_tar_gz("./pkg/bin/tool", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Pandoc, "pkg/bin/tool");
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.gz",
            sha256: "0",
            packaging: Packaging::TarGz,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    /// Builds a tiny in-memory `.tar.xz` with one member, by tarring it
    /// in-memory and then compressing with `lzma_rs::xz_compress` — the
    /// exact format `extract_tar_xz` decodes (in reverse), so this exercises
    /// the real xz container without a network fetch and without shelling
    /// out to an external `xz` binary.
    fn make_test_tar_xz(member: &str, contents: &[u8]) -> Vec<u8> {
        let tar_bytes = {
            let mut buf = Vec::new();
            let mut builder = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, member, Cursor::new(contents))
                .unwrap();
            builder.into_inner().unwrap();
            buf
        };
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(&tar_bytes[..]), &mut out).unwrap();
        out
    }

    #[test]
    fn extract_tar_xz_finds_the_named_member() {
        let bytes = make_test_tar_xz(
            "typst-x86_64-unknown-linux-musl/typst",
            b"pretend-exe-bytes",
        );
        let member = one_member(
            crate::Backend::Typst,
            "typst-x86_64-unknown-linux-musl/typst",
        );
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.xz",
            sha256: "0",
            packaging: Packaging::TarXz,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    /// Same tolerance `extract_tar_gz` has: a tar entry name with a leading
    /// `./` must still match a manifest `archive_member` that has none.
    #[test]
    fn extract_tar_xz_tolerates_a_leading_dot_slash_in_the_entry_name() {
        let bytes = make_test_tar_xz("./pkg/bin/tool", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Typst, "pkg/bin/tool");
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.xz",
            sha256: "0",
            packaging: Packaging::TarXz,
            members: leaked(vec![member]),
            version: "0",
        };
        let out = extract(&asset, &member, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    #[test]
    fn extract_tar_xz_reports_a_missing_member() {
        let bytes = make_test_tar_xz("pkg/bin/tool", b"pretend-exe-bytes");
        let member = one_member(crate::Backend::Typst, "pkg/bin/other-tool");
        let asset = Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.xz",
            sha256: "0",
            packaging: Packaging::TarXz,
            members: leaked(vec![member]),
            version: "0",
        };
        let e = extract(&asset, &member, &bytes).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
    }

    /// `CappedWriter` is the xz decompression path's equivalent of
    /// `read_capped`'s `take(cap + 1)` trick: it must reject the exact write
    /// that would push it over the cap, using a tiny local cap rather than
    /// the real 512 MiB `MAX_DOWNLOAD_BYTES` so the test itself stays cheap.
    #[test]
    fn capped_writer_rejects_a_write_that_would_exceed_the_cap() {
        let mut w = CappedWriter::new(4);
        std::io::Write::write_all(&mut w, b"abcd").unwrap();
        assert!(std::io::Write::write_all(&mut w, b"e").is_err());
    }

    #[test]
    fn capped_writer_accepts_writes_up_to_and_including_the_cap() {
        let mut w = CappedWriter::new(4);
        assert!(std::io::Write::write_all(&mut w, b"abcd").is_ok());
        assert_eq!(w.buf, b"abcd");
    }

    /// Exercises the exact `take(cap + 1)` pattern `read_capped` uses
    /// internally — without allocating anywhere near `MAX_DOWNLOAD_BYTES`
    /// (512 MiB) in a unit test — by wrapping a reader that's one byte over
    /// a tiny local cap and confirming the overflow is observable.
    #[test]
    fn read_capped_pattern_detects_a_reader_over_the_cap() {
        let tiny_cap: u64 = 4;
        let data = [0u8; 5]; // one byte over `tiny_cap`
        let mut out = Vec::new();
        let mut limited = data.as_slice().take(tiny_cap + 1);
        std::io::Read::read_to_end(&mut limited, &mut out).unwrap();
        assert!(
            out.len() as u64 > tiny_cap,
            "the take(cap + 1) pattern must let an over-cap reader be detected"
        );
    }

    #[test]
    fn read_capped_accepts_data_under_the_cap() {
        let out = read_capped(b"small".as_slice()).unwrap();
        assert_eq!(out, b"small");
    }

    /// Review finding 1: the atomic write/finalize/rename sequence, on its
    /// happy path — no network involved, since `install_bytes` is the tail
    /// of `fetch_and_install` that starts after the bytes are already in
    /// hand.
    #[test]
    fn install_bytes_writes_dest_and_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir
            .path()
            .join(if cfg!(windows) { "tool.exe" } else { "tool" });

        install_bytes(&dest, b"pretend-exe-bytes").unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"pretend-exe-bytes");
        assert!(
            !temp_path_for(&dest).exists(),
            "the .part file must not survive a successful install"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    /// Review finding 1's regression test: when the step *after* a
    /// successful write fails — here, the final rename, because `dest` is
    /// already an existing directory rather than a plain file — nothing is
    /// left at `dest` and the temp file is cleaned up. This is the same
    /// safety property a failing `codesign` on macOS arm64 depends on: the
    /// write and `finalize` (chmod / ad-hoc sign) always happen at the
    /// `.part` path, so *any* later failure — rename included — is caught
    /// before `dest` is ever touched.
    #[test]
    fn install_bytes_leaves_dest_untouched_when_the_final_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tool"); // a directory, not a plain file
        std::fs::create_dir(&dest).unwrap();

        let result = install_bytes(&dest, b"pretend-exe-bytes");

        assert!(
            result.is_err(),
            "renaming a file over an existing directory must fail"
        );
        assert!(dest.is_dir(), "dest must be left exactly as it was");
        assert!(
            !temp_path_for(&dest).exists(),
            "the .part file must be cleaned up when the rename fails"
        );
    }

    /// A failure earlier still — the write itself, here forced by pointing
    /// `dest`'s parent at a path that is a file rather than a directory —
    /// must leave nothing behind either, and must never reach the rename.
    #[test]
    fn install_bytes_leaves_no_temp_file_when_the_write_itself_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-directory");
        std::fs::write(&blocking_file, b"in the way").unwrap();
        let dest = blocking_file.join("ffmpeg"); // parent is a file, not a dir

        let result = install_bytes(&dest, b"pretend-exe-bytes");

        assert!(
            result.is_err(),
            "writing under a file-as-directory must fail"
        );
        assert!(
            !dest.exists(),
            "dest must never be created when the write fails"
        );
    }

    // --- Folder installs ---------------------------------------------------

    enum Item {
        File(&'static [u8]),
        Link(&'static str),
        Dir,
    }

    fn make_folder_zip(items: &[(&str, Item)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let options = zip::write::SimpleFileOptions::default();
            for (name, item) in items {
                match item {
                    Item::File(bytes) => {
                        w.start_file(*name, options).unwrap();
                        std::io::Write::write_all(&mut w, bytes).unwrap();
                    }
                    Item::Link(target) => w.add_symlink(*name, *target, options).unwrap(),
                    Item::Dir => w.add_directory(*name, options).unwrap(),
                }
            }
            w.finish().unwrap();
        }
        buf
    }

    fn folder_asset(archive_member: &'static str) -> Asset {
        Asset {
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/qpdf.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            members: leaked(vec![one_member(crate::Backend::Qpdf, archive_member)]),
            version: "0",
        }
    }

    fn program_dest(managed: &Path) -> PathBuf {
        let exe = if cfg!(windows) { "qpdf.exe" } else { "qpdf" };
        managed.join("qpdf").join("bin").join(exe)
    }

    fn no_check(_: crate::Backend, _: &Path) -> Result<()> {
        Ok(())
    }

    fn entries_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn keep_in_folder_takes_only_runtime_files() {
        for (rel, kept) in [
            ("bin/qpdf", true),
            ("bin/qpdf30.dll", true),
            ("bin/VCRUNTIME140.DLL", true),
            ("lib/libqpdf.30.4.2.dylib", true),
            ("lib/libqpdf.so.30", true),
            ("lib/libffi.so.8", true),
            ("lib/libx.so", true),
            ("bin/fix-qdf", false),
            ("bin/zlib-flate.exe", false),
            ("lib/libqpdf.a", false),
            ("lib/qpdf.lib", false),
            ("lib/pkgconfig/libqpdf.pc", false),
            ("lib/cmake/qpdf/qpdfConfig.cmake", false),
            ("include/qpdf/QPDF.hh", false),
            ("share/doc/qpdf/README.md", false),
            ("bin/..\\..\\evil.dll", false),
        ] {
            assert_eq!(keep_in_folder(rel, "bin/qpdf"), kept, "{rel}");
        }
    }

    #[test]
    fn link_targets_must_stay_inside_the_folder() {
        for (link, target, ok) in [
            ("lib/libqpdf.so.30", "libqpdf.so.30.4.2", true),
            ("lib/a.so.1", "./a.so.1.0", true),
            ("lib/a.so.1", "../lib/a.so.1.0", true),
            ("lib/a.so.1", "../../outside", false),
            ("lib/a.so.1", "/etc/passwd", false),
            ("lib/a.so.1", "..", false),
        ] {
            assert_eq!(
                link_stays_inside(Path::new(link), target),
                ok,
                "{link} -> {target}"
            );
        }
    }

    #[test]
    fn a_folder_install_keeps_the_program_and_its_libraries_and_nothing_else() {
        let zip = make_folder_zip(&[
            ("bin/", Item::Dir),
            ("bin/qpdf", Item::File(b"program")),
            ("bin/fix-qdf", Item::File(b"other tool")),
            ("lib/libqpdf.so.30.4.2", Item::File(b"library")),
            ("lib/libqpdf.a", Item::File(b"static")),
            ("lib/pkgconfig/libqpdf.pc", Item::File(b"pc")),
            ("include/qpdf/QPDF.hh", Item::File(b"header")),
        ]);
        let managed = tempfile::tempdir().unwrap();
        let dest = program_dest(managed.path());

        let installed =
            install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &no_check).unwrap();

        assert_eq!(installed, vec![(crate::Backend::Qpdf, dest.clone())]);
        let folder = managed.path().join("qpdf");
        assert_eq!(std::fs::read(folder.join("bin/qpdf")).unwrap(), b"program");
        assert_eq!(
            std::fs::read(folder.join("lib/libqpdf.so.30.4.2")).unwrap(),
            b"library"
        );
        for skipped in ["bin/fix-qdf", "lib/libqpdf.a", "lib/pkgconfig", "include"] {
            assert!(
                !folder.join(skipped).exists(),
                "{skipped} must not be installed"
            );
        }
        assert_eq!(
            entries_in(managed.path()),
            vec!["qpdf"],
            "no temp folder may remain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_install_recreates_library_symlinks() {
        let zip = make_folder_zip(&[
            ("bin/qpdf", Item::File(b"program")),
            ("lib/libqpdf.so.30.4.2", Item::File(b"library")),
            ("lib/libqpdf.so.30", Item::Link("libqpdf.so.30.4.2")),
        ]);
        let managed = tempfile::tempdir().unwrap();
        let dest = program_dest(managed.path());

        install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &no_check).unwrap();

        let link = managed.path().join("qpdf/lib/libqpdf.so.30");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("libqpdf.so.30.4.2")
        );
        assert_eq!(std::fs::read(&link).unwrap(), b"library");
    }

    #[test]
    fn a_symlink_pointing_outside_the_folder_is_refused() {
        let zip = make_folder_zip(&[
            ("bin/qpdf", Item::File(b"program")),
            ("lib/libevil.so.1", Item::Link("../../../outside")),
        ]);
        let managed = tempfile::tempdir().unwrap();
        let dest = program_dest(managed.path());

        let err = install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &no_check)
            .unwrap_err();

        assert!(
            err.message.contains("outside its folder"),
            "{}",
            err.message
        );
        assert!(
            entries_in(managed.path()).is_empty(),
            "nothing may be written"
        );
    }

    #[test]
    fn a_folder_install_strips_the_archive_root_and_keeps_dlls_beside_the_program() {
        let zip = make_folder_zip(&[
            ("qpdf-12.4.2-msvc64/bin/qpdf.exe", Item::File(b"program")),
            ("qpdf-12.4.2-msvc64/bin/qpdf30.dll", Item::File(b"library")),
            (
                "qpdf-12.4.2-msvc64/bin/zlib-flate.exe",
                Item::File(b"other tool"),
            ),
            (
                "qpdf-12.4.2-msvc64/lib/qpdf_static.lib",
                Item::File(b"static"),
            ),
        ]);
        let managed = tempfile::tempdir().unwrap();
        let dest = program_dest(managed.path());

        install_verified(
            &folder_asset("qpdf-12.4.2-msvc64/bin/qpdf.exe"),
            &zip,
            |_| dest.clone(),
            &no_check,
        )
        .unwrap();

        let folder = managed.path().join("qpdf");
        assert_eq!(
            std::fs::read(folder.join("bin/qpdf.exe")).unwrap(),
            b"program"
        );
        assert_eq!(
            std::fs::read(folder.join("bin/qpdf30.dll")).unwrap(),
            b"library"
        );
        assert!(!folder.join("bin/zlib-flate.exe").exists());
        assert!(!folder.join("lib").exists());
    }

    #[test]
    fn a_folder_archive_without_the_program_installs_nothing() {
        let zip = make_folder_zip(&[("lib/libqpdf.so.30.4.2", Item::File(b"library"))]);
        let managed = tempfile::tempdir().unwrap();
        let dest = program_dest(managed.path());

        let err = install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &no_check)
            .unwrap_err();

        assert!(err.message.contains("bin/qpdf"), "{}", err.message);
        assert!(entries_in(managed.path()).is_empty());
    }

    #[test]
    fn a_failing_run_check_leaves_the_installed_folder_as_it_was() {
        let zip = make_folder_zip(&[("bin/qpdf", Item::File(b"new program"))]);
        let managed = tempfile::tempdir().unwrap();
        let folder = managed.path().join("qpdf");
        std::fs::create_dir_all(folder.join("bin")).unwrap();
        std::fs::write(folder.join("bin/qpdf"), b"old program").unwrap();
        let dest = program_dest(managed.path());
        let failing = |_: crate::Backend, _: &Path| -> Result<()> {
            Err(ConvError::new(ErrorCode::ConversionFailed, "did not run"))
        };

        let err = install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &failing)
            .unwrap_err();

        assert_eq!(err.message, "did not run");
        assert_eq!(
            std::fs::read(folder.join("bin/qpdf")).unwrap(),
            b"old program"
        );
        assert_eq!(entries_in(managed.path()), vec!["qpdf"]);
    }

    #[test]
    fn a_folder_install_replaces_the_previous_folder_entirely() {
        let zip = make_folder_zip(&[("bin/qpdf", Item::File(b"new program"))]);
        let managed = tempfile::tempdir().unwrap();
        let folder = managed.path().join("qpdf");
        std::fs::create_dir_all(folder.join("bin")).unwrap();
        std::fs::write(folder.join("bin/qpdf"), b"old program").unwrap();
        std::fs::write(folder.join("stale.txt"), b"from an older version").unwrap();
        let dest = program_dest(managed.path());

        install_verified(&folder_asset("bin/qpdf"), &zip, |_| dest.clone(), &no_check).unwrap();

        assert_eq!(
            std::fs::read(folder.join("bin/qpdf")).unwrap(),
            b"new program"
        );
        assert!(!folder.join("stale.txt").exists());
        assert_eq!(entries_in(managed.path()), vec!["qpdf"]);
    }
}
