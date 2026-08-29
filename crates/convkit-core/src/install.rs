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
use crate::manifest::{Asset, Format};

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

/// GETs `url` into memory. No redirects to worry about beyond what `ureq`
/// follows by default — GitHub release-asset URLs redirect once, to S3 or
/// Azure blob storage, which `ureq` follows transparently.
fn download(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(300))
        .call()
        .map_err(|e| download_err(url, e))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| download_err(url, e))?;
    Ok(bytes)
}

fn extract_zip(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| extract_err(asset, e))?;
    let mut file = archive
        .by_name(asset.archive_member)
        .map_err(|e| extract_err(asset, format!("{} not found: {e}", asset.archive_member)))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)
        .map_err(|e| extract_err(asset, e))?;
    Ok(out)
}

fn extract_tar_gz(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|e| extract_err(asset, e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| extract_err(asset, e))?;
        let path = entry.path().map_err(|e| extract_err(asset, e))?;
        if path.to_string_lossy() == asset.archive_member {
            let mut out = Vec::new();
            entry
                .read_to_end(&mut out)
                .map_err(|e| extract_err(asset, e))?;
            return Ok(out);
        }
    }
    Err(extract_err(
        asset,
        format!("{} not found in archive", asset.archive_member),
    ))
}

fn extract(asset: &Asset, bytes: &[u8]) -> Result<Vec<u8>> {
    match asset.format {
        Format::Raw => Ok(bytes.to_vec()),
        Format::Zip => extract_zip(asset, bytes),
        Format::TarGz => extract_tar_gz(asset, bytes),
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

/// Downloads `asset`, verifies its checksum, extracts the executable, and
/// writes it into `dest_dir` (created if needed) under the platform's
/// canonical filename for `asset.backend` — the same name
/// `Resolver::candidates` looks for under `Resolver::managed_dir()`. Returns
/// the path actually written.
pub fn fetch_and_install(asset: &Asset, dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir).map_err(|e| io_err(dest_dir, e))?;

    let bytes = download(asset.url)?;
    verify(&bytes, asset.sha256)?;
    let exe_bytes = extract(asset, &bytes)?;

    let filename = if cfg!(windows) {
        format!("{}.exe", asset.backend.exe_name())
    } else {
        asset.backend.exe_name().to_string()
    };
    let dest = dest_dir.join(filename);
    std::fs::write(&dest, &exe_bytes).map_err(|e| io_err(&dest, e))?;
    finalize(&dest)?;

    Ok(dest)
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
            format: Format::Raw,
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
            format: Format::Zip,
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
            format: Format::Zip,
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
            format: Format::TarGz,
            archive_member: "pkg/bin/tool",
        };
        let out = extract(&asset, &bytes).unwrap();
        assert_eq!(out, b"pretend-exe-bytes");
    }
}
