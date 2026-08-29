//! Downloads, verifies, and unpacks a managed backend's binary.
//!
//! This module never prints anything — like the rest of `convkit-core`, all
//! progress reporting belongs to the `conv` binary. It also never touches a
//! backend it isn't handed a verified [`manifest::Asset`] for; deciding
//! *which* asset to use (or refusing when none exists) is the caller's job.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{ConvError, ErrorCode, Result};
use crate::manifest::{Asset, Packaging};

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

fn extract_err(asset: &Asset, e: impl std::fmt::Display) -> ConvError {
    ConvError::new(
        ErrorCode::ConversionFailed,
        format!("failed to extract {}: {e}", asset.url),
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

fn extract_zip(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| extract_err(asset, e))?;
    let file = archive
        .by_name(asset.archive_member)
        .map_err(|e| extract_err(asset, format!("{} not found: {e}", asset.archive_member)))?;
    read_capped(file).map_err(|e| extract_err(asset, e))
}

/// A tar entry's path as stored may carry a leading `./` (many tar tools
/// write GNU-format entries this way); the manifest's `archive_member`
/// values never do, so normalise both sides the same way rather than
/// requiring the manifest to guess the exact byte-for-byte spelling a given
/// tarball uses.
fn strip_leading_dot_slash(s: &str) -> &str {
    s.strip_prefix("./").unwrap_or(s)
}

fn extract_tar_gz(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|e| extract_err(asset, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| extract_err(asset, e))?;
        let path = entry.path().map_err(|e| extract_err(asset, e))?;
        let name = path.to_string_lossy();
        if strip_leading_dot_slash(&name) == asset.archive_member {
            return read_capped(entry).map_err(|e| extract_err(asset, e));
        }
    }
    Err(extract_err(
        asset,
        format!("{} not found in archive", asset.archive_member),
    ))
}

fn extract(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    match asset.packaging {
        Packaging::Raw => Ok(bytes.to_vec()),
        Packaging::Zip => extract_zip(asset, bytes),
        Packaging::TarGz => extract_tar_gz(asset, bytes),
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

/// Downloads `asset`, verifies its checksum, extracts the executable, and
/// writes it to `dest` — the exact final path, typically
/// `Resolver::managed_path(backend)`. Returns `dest` on success. See
/// `install_bytes` for the atomicity guarantee this provides.
pub fn fetch_and_install(asset: &Asset, dest: &Path) -> Result<PathBuf> {
    let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dest_dir).map_err(|e| io_err(dest_dir, e))?;

    let bytes = download(asset.url)?;
    verify(&bytes, asset.sha256)?;
    let exe_bytes = extract(asset, &bytes)?;

    install_bytes(dest, &exe_bytes)?;
    Ok(dest.to_path_buf())
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

    #[test]
    fn extract_raw_returns_the_bytes_unchanged() {
        let asset = Asset {
            backend: crate::Backend::Ffmpeg,
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/ffmpeg",
            sha256: "0",
            packaging: Packaging::Raw,
            archive_member: "",
        };
        let out = extract(&asset, b"not-really-an-executable").unwrap();
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
        let asset = Asset {
            backend: crate::Backend::Ffmpeg,
            os: "windows",
            arch: "x64",
            url: "https://example.invalid/tool.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            archive_member: "bin/tool.exe",
        };
        let out = extract(&asset, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }

    #[test]
    fn extract_zip_reports_a_missing_member() {
        let bytes = make_test_zip("bin/tool.exe", b"pretend-exe-bytes");
        let asset = Asset {
            backend: crate::Backend::Ffmpeg,
            os: "windows",
            arch: "x64",
            url: "https://example.invalid/tool.zip",
            sha256: "0",
            packaging: Packaging::Zip,
            archive_member: "bin/other.exe",
        };
        let e = extract(&asset, &bytes).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
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
        let asset = Asset {
            backend: crate::Backend::Pandoc,
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.gz",
            sha256: "0",
            packaging: Packaging::TarGz,
            archive_member: "pkg/bin/tool",
        };
        let out = extract(&asset, &bytes).unwrap();
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
        let asset = Asset {
            backend: crate::Backend::Pandoc,
            os: "linux",
            arch: "x64",
            url: "https://example.invalid/tool.tar.gz",
            sha256: "0",
            packaging: Packaging::TarGz,
            archive_member: "pkg/bin/tool",
        };
        let out = extract(&asset, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
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
}
