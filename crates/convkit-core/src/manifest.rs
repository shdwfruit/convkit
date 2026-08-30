//! Pinned, checksummed download manifest for `conv install`.
//!
//! Every entry below was populated by actually downloading the asset and
//! hashing the bytes that came back — never invented from a URL that looked
//! right. See Task 14's report for the exact commands used to derive each
//! one. Two rules the tests in this module enforce mechanically:
//!
//! - No `url` may point at a GitHub `releases/latest` alias. That endpoint
//!   redirects through the REST API, which is rate-limited to 60
//!   requests/hour *per IP* — shared by everyone behind one office NAT — so
//!   every entry here names a specific, immutable release tag instead.
//! - `Backend::Soffice` never appears. LibreOffice has no relocatable
//!   binary; `Backend::is_managed()` already says so, and this manifest
//!   agrees by omission rather than by a runtime check anyone could forget.
//!
//! `ImageMagick` (`Backend::Magick`) is also absent, on every platform: its
//! official releases ship Windows portable builds only as `.7z`, Linux only
//! as an AppImage (which needs FUSE to run without an extra `--appimage-
//! extract-and-run` flag this manifest has no way to pass), and no
//! standalone macOS binary at all. None of those clear the "verified from a
//! plain `.zip`/`.tar.gz` download" bar this manifest holds every entry to,
//! so `magick` is left for the user's package manager, same as `soffice`.

use crate::Backend;

/// How the downloaded bytes are packaged, and therefore how
/// `install::fetch_and_install` gets each executable out of them. Named
/// `Packaging` rather than `Format` specifically to avoid colliding with
/// (and being confused for) the crate-root `Format` type, which is the
/// unrelated media-format enum (`Mp4`, `Jpg`, `Pdf`, ...) the rest of the
/// crate uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packaging {
    /// A zip archive; each member's `archive_member` is its path inside it.
    Zip,
    /// A gzip-compressed tarball; each member's `archive_member` is its path
    /// inside it.
    TarGz,
    /// An xz-compressed tarball; each member's `archive_member` is its path
    /// inside it. Decoded with `lzma-rs`, a pure-Rust xz/LZMA decoder — not
    /// `xz2`/`liblzma-sys`, which link a C library and would reintroduce the
    /// exact C-toolchain fragility on Windows that ruled out `.7z` in the
    /// first place.
    TarXz,
    /// The downloaded bytes are the executable, verbatim — no archive at
    /// all. A `Raw` asset therefore has exactly one member, and that
    /// member's `archive_member` is unused (empty).
    Raw,
}

/// One executable this asset's download provisions, and the [`Backend`] it
/// satisfies. Most assets name exactly one; an asset whose upstream release
/// bundles several tools in one archive (today: ffmpeg + ffprobe on
/// Windows) names one `ArchiveMember` per tool, so `install::
/// fetch_and_install` extracts and installs all of them from the single
/// verified download rather than asking the caller to fetch the same bytes
/// again for the second binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveMember {
    pub backend: Backend,
    /// Path of the executable inside the archive; empty for
    /// `Packaging::Raw`, where the whole download *is* this one member.
    pub archive_member: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// `"windows"`, `"macos"`, or `"linux"` — matches [`current_os`].
    pub os: &'static str,
    /// `"x64"` or `"arm64"` — matches [`current_arch`].
    pub arch: &'static str,
    /// A pinned, immutable release-asset URL. Never `releases/latest`.
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the exact bytes at `url`, 64 characters.
    pub sha256: &'static str,
    pub packaging: Packaging,
    /// Every executable this one download provisions. Never empty; `Raw`
    /// packaging always has exactly one.
    pub members: &'static [ArchiveMember],
}

/// Every verified (backend, platform) pair `conv install` can provision.
/// Deliberately not exhaustive — see the module docs for what's missing and
/// why. [`lookup`] is the intended way to read this; it's `pub` mainly so
/// tests can walk every entry and check its shape.
pub static ALL: &[Asset] = &[
    // --- ffmpeg + ffprobe: Windows x64 ----------------------------------
    // GyanD/codexffmpeg release 9.0.1 — a real version tag (not "latest"),
    // essentials build, which bundles ffmpeg.exe and ffprobe.exe together in
    // one zip. One `Asset`, two members: `conv install ffmpeg` and `conv
    // install ffprobe` both resolve to this entry and both provision both
    // binaries from the one download — see `install::fetch_and_install`.
    Asset {
        os: "windows",
        arch: "x64",
        url: "https://github.com/GyanD/codexffmpeg/releases/download/9.0.1/ffmpeg-9.0.1-essentials_build.zip",
        sha256: "fec81ae03971d9dd4be3ebe02e263bd2ec1d789483f931bdba5f5715e65da2e9",
        packaging: Packaging::Zip,
        members: &[
            ArchiveMember {
                backend: Backend::Ffmpeg,
                archive_member: "ffmpeg-9.0.1-essentials_build/bin/ffmpeg.exe",
            },
            ArchiveMember {
                backend: Backend::Ffprobe,
                archive_member: "ffmpeg-9.0.1-essentials_build/bin/ffprobe.exe",
            },
        ],
    },
    // --- ffmpeg / ffprobe: Linux x64, macOS x64, macOS arm64 -----------
    // eugeneware/ffmpeg-static release `b6.1.1` — a pinned tag, ffmpeg
    // 6.1.1. Assets are raw statically-linked executables, not archives:
    // downloaded and confirmed by `file(1)` to be real ELF/Mach-O binaries
    // at the expected size, not gzip streams mislabeled by extension.
    // Unlike the Windows zip above, upstream publishes ffmpeg and ffprobe
    // here as two genuinely separate files at two separate URLs — there is
    // no single shared download to provision both from, so these stay two
    // one-member `Asset`s, same as before.
    Asset {
        os: "linux",
        arch: "x64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-linux-x64",
        sha256: "e7e7fb30477f717e6f55f9180a70386c62677ef8a4d4d1a5d948f4098aa3eb99",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffmpeg,
            archive_member: "",
        }],
    },
    Asset {
        os: "linux",
        arch: "x64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-linux-x64",
        sha256: "4f231a1960d83e403d08f7971e271707bec278a9ae18e21b8b5b03186668450d",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffprobe,
            archive_member: "",
        }],
    },
    Asset {
        os: "macos",
        arch: "x64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-x64",
        sha256: "ebdddc936f61e14049a2d4b549a412b8a40deeff6540e58a9f2a2da9e6b18894",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffmpeg,
            archive_member: "",
        }],
    },
    Asset {
        os: "macos",
        arch: "x64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-darwin-x64",
        sha256: "fa3add0ce901f7241abe0dfc0155d958fc834aca3f8ce61f87cc712ae669c1e0",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffprobe,
            archive_member: "",
        }],
    },
    Asset {
        os: "macos",
        arch: "arm64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-arm64",
        sha256: "a90e3db6a3fd35f6074b013f948b1aa45b31c6375489d39e572bea3f18336584",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffmpeg,
            archive_member: "",
        }],
    },
    Asset {
        os: "macos",
        arch: "arm64",
        url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffprobe-darwin-arm64",
        sha256: "bb2db6f5d8cef919da12fbf592119a987202a8c060a886f3cab091f9cab90b64",
        packaging: Packaging::Raw,
        members: &[ArchiveMember {
            backend: Backend::Ffprobe,
            archive_member: "",
        }],
    },
    // --- pandoc: all four platforms -------------------------------------
    // jgm/pandoc release 3.11 — a real version tag, not "latest".
    Asset {
        os: "windows",
        arch: "x64",
        url: "https://github.com/jgm/pandoc/releases/download/3.11/pandoc-3.11-windows-x86_64.zip",
        sha256: "2ab72baf2399450e148ddf7a2a8689806c42e1bba71862b57e220fd9b8456d3d",
        packaging: Packaging::Zip,
        members: &[ArchiveMember {
            backend: Backend::Pandoc,
            archive_member: "pandoc-3.11/pandoc.exe",
        }],
    },
    Asset {
        os: "linux",
        arch: "x64",
        url: "https://github.com/jgm/pandoc/releases/download/3.11/pandoc-3.11-linux-amd64.tar.gz",
        sha256: "37edb3bbcf722f921a009941bf5874e2e0c09263226c9b4a2d980788cb062ab6",
        packaging: Packaging::TarGz,
        members: &[ArchiveMember {
            backend: Backend::Pandoc,
            archive_member: "pandoc-3.11/bin/pandoc",
        }],
    },
    Asset {
        os: "macos",
        arch: "x64",
        url: "https://github.com/jgm/pandoc/releases/download/3.11/pandoc-3.11-x86_64-macOS.zip",
        sha256: "3b1c1b57f160112c821d02f23d946ede8b7f57a6ccf4632a25a512d334a9291f",
        packaging: Packaging::Zip,
        members: &[ArchiveMember {
            backend: Backend::Pandoc,
            archive_member: "pandoc-3.11-x86_64/bin/pandoc",
        }],
    },
    Asset {
        os: "macos",
        arch: "arm64",
        url: "https://github.com/jgm/pandoc/releases/download/3.11/pandoc-3.11-arm64-macOS.zip",
        sha256: "15806bedf9517bfead72e88fe6a6696635c3691efbb6e152173440e9c5bb50b4",
        packaging: Packaging::Zip,
        members: &[ArchiveMember {
            backend: Backend::Pandoc,
            archive_member: "pandoc-3.11-arm64/bin/pandoc",
        }],
    },
    // --- typst: all four platforms --------------------------------------
    // typst/typst release 0.15.1 — a real version tag, not "latest". Linux
    // and macOS assets are `.tar.xz` (musl-static on Linux, so it runs
    // regardless of the host glibc); see `Packaging::TarXz`'s docs for why
    // that's decoded with `lzma-rs` rather than a C-linking crate. Every
    // digest below was computed from an actual download, and the Windows
    // digest independently matches the hash published in Scoop's own
    // `typst.json` manifest (ScoopInstaller/Main), a second source agreeing
    // with the one this manifest was built from.
    Asset {
        os: "windows",
        arch: "x64",
        url: "https://github.com/typst/typst/releases/download/v0.15.1/typst-x86_64-pc-windows-msvc.zip",
        sha256: "19ce3551153c2fe7ee9fa2f95208310c8f4d3209fedb699e0333faf8913f6736",
        packaging: Packaging::Zip,
        members: &[ArchiveMember {
            backend: Backend::Typst,
            archive_member: "typst-x86_64-pc-windows-msvc/typst.exe",
        }],
    },
    Asset {
        os: "linux",
        arch: "x64",
        url: "https://github.com/typst/typst/releases/download/v0.15.1/typst-x86_64-unknown-linux-musl.tar.xz",
        sha256: "a6d077d0a95eed5a2eba715b2dae06be954f624ccbf85758a03f389ded33118c",
        packaging: Packaging::TarXz,
        members: &[ArchiveMember {
            backend: Backend::Typst,
            archive_member: "typst-x86_64-unknown-linux-musl/typst",
        }],
    },
    Asset {
        os: "macos",
        arch: "x64",
        url: "https://github.com/typst/typst/releases/download/v0.15.1/typst-x86_64-apple-darwin.tar.xz",
        sha256: "7f9fdd9584866245de9a79e0add8f9236fae6f40a8a45e2c4771ccc14db4e0fa",
        packaging: Packaging::TarXz,
        members: &[ArchiveMember {
            backend: Backend::Typst,
            archive_member: "typst-x86_64-apple-darwin/typst",
        }],
    },
    Asset {
        os: "macos",
        arch: "arm64",
        url: "https://github.com/typst/typst/releases/download/v0.15.1/typst-aarch64-apple-darwin.tar.xz",
        sha256: "48f62ed034aa3a7978309579ac6ca00045e2ef0da73114e8af27cfd8e74dc05a",
        packaging: Packaging::TarXz,
        members: &[ArchiveMember {
            backend: Backend::Typst,
            archive_member: "typst-aarch64-apple-darwin/typst",
        }],
    },
];

/// The running process's OS, in the vocabulary this manifest uses.
pub fn current_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// The running process's CPU architecture, in the vocabulary this manifest
/// uses. Anything other than the two architectures this manifest covers
/// falls back to `"unknown"`, which simply matches no entry — `lookup`
/// reports that the same way it reports any other unverified platform.
pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    }
}

/// The verified asset for `backend` on the platform this process is running
/// on, if one exists. `None` means: not that the backend can't be found,
/// but that this manifest has no download verified for this exact
/// (backend, OS, arch) triple — the caller should fall back to the manual
/// install hint, not attempt a download anyway.
///
/// When the returned asset's `members` names more than one backend (today:
/// the Windows ffmpeg/ffprobe zip), `lookup(Backend::Ffmpeg)` and
/// `lookup(Backend::Ffprobe)` both return the *same* `Asset` — that's the
/// whole mechanism `install::fetch_and_install` relies on to provision both
/// from a single download, regardless of which of the two names was asked
/// for.
pub fn lookup(backend: Backend) -> Option<&'static Asset> {
    let os = current_os();
    let arch = current_arch();
    ALL.iter()
        .find(|a| a.os == os && a.arch == arch && a.members.iter().any(|m| m.backend == backend))
}

/// Every other backend that installing `backend` also provisions, because
/// they share one manifest entry — empty when `backend` has no manifest
/// entry at all, or when its entry names only itself. Purely a data query
/// over [`ALL`]; used to tell the user, before or after a download, what
/// else they're getting (`conv install`'s success report, and the
/// install-and-retry prompt).
pub fn bundled_with(backend: Backend) -> Vec<Backend> {
    match lookup(backend) {
        Some(asset) => asset
            .members
            .iter()
            .map(|m| m.backend)
            .filter(|&b| b != backend)
            .collect(),
        None => Vec::new(),
    }
}

/// True when `conv install <backend>` can actually provision this backend
/// on the platform this process is running on right now.
///
/// `Backend::is_managed()` alone is not enough to answer that: it is a
/// static, platform-independent policy predicate (false only for
/// `Soffice`, which has no relocatable binary on *any* platform, ever).
/// `Backend::Magick` is `is_managed() == true` — a managed install is
/// architecturally possible in principle — but this manifest verifies zero
/// platforms for it (see the module docs above: every official release is
/// a `.7z`, an AppImage, or has no standalone build at all), and
/// `ffmpeg`/`pandoc` are only verified for four of the platform/arch pairs
/// this project ships binaries for — there is no `linux`/`arm64` row, for
/// instance, even though `dist-workspace.toml` builds that target.
/// `is_managed() == true` with no manifest entry therefore means "not
/// verified on this platform," which is exactly the gap this predicate
/// closes.
///
/// This is what any user-facing "can `conv install` fix this" surface must
/// call — `ConvError::backend_missing`'s remediation and `doctor`'s
/// `managed_install` field — so it never promises an install that will
/// immediately refuse. `Backend::is_managed()` itself stays the right
/// check for `commands/install.rs`, which needs the coarser, static
/// distinction between "no relocatable build, permanent policy"
/// (`ConvError::not_installable`) and "not verified on this platform yet"
/// (`ConvError::no_managed_build`) — collapsing the two predicates here
/// would erase that distinction and start telling users ffmpeg has no
/// relocatable build at all, which is false.
pub fn has_managed_build(backend: Backend) -> bool {
    backend.is_managed() && lookup(backend).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libreoffice_has_no_manifest_entry() {
        assert!(ALL
            .iter()
            .all(|a| a.members.iter().all(|m| m.backend != Backend::Soffice)));
    }

    #[test]
    fn imagemagick_has_no_manifest_entry() {
        // Every official ImageMagick asset that could stand in for `magick`
        // is either a `.7z` (Windows) or an AppImage (Linux) with no plain
        // `.zip`/`.tar.gz` alternative — see the module docs.
        assert!(ALL
            .iter()
            .all(|a| a.members.iter().all(|m| m.backend != Backend::Magick)));
    }

    #[test]
    fn every_manifest_url_is_pinned_not_latest() {
        for a in ALL {
            assert!(
                a.url.starts_with("https://"),
                "{} is not an https:// URL",
                a.url
            );
            assert!(
                !a.url.contains("releases/latest"),
                "{} uses an unpinned URL",
                a.url
            );
            assert_eq!(a.sha256.len(), 64, "{} has no pinned digest", a.url);
            assert!(
                a.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} digest is not hex: {}",
                a.url,
                a.sha256
            );
            assert!(
                a.sha256.chars().all(|c| !c.is_ascii_uppercase()),
                "{} digest must be lowercase: {}",
                a.url,
                a.sha256
            );
        }
    }

    #[test]
    fn every_archive_entry_names_a_member_and_every_raw_entry_does_not() {
        for a in ALL {
            for m in a.members {
                match a.packaging {
                    Packaging::Raw => assert!(
                        m.archive_member.is_empty(),
                        "{} is Raw but names archive_member {:?}",
                        a.url,
                        m.archive_member
                    ),
                    Packaging::Zip | Packaging::TarGz | Packaging::TarXz => assert!(
                        !m.archive_member.is_empty(),
                        "{} is an archive with no archive_member for {:?}",
                        a.url,
                        m.backend
                    ),
                }
            }
        }
    }

    #[test]
    fn every_entry_names_at_least_one_member() {
        for a in ALL {
            assert!(!a.members.is_empty(), "{} names no members", a.url);
        }
    }

    /// A `Raw` asset's downloaded bytes *are* the one executable — there is
    /// no archive to pull a second member out of, so unlike `Zip`/`TarGz`/
    /// `TarXz`, a `Raw` entry can never bundle more than one backend.
    #[test]
    fn a_raw_asset_names_exactly_one_member() {
        for a in ALL {
            if a.packaging == Packaging::Raw {
                assert_eq!(
                    a.members.len(),
                    1,
                    "{} is Raw but names {} members",
                    a.url,
                    a.members.len()
                );
            }
        }
    }

    #[test]
    fn no_two_entries_claim_the_same_backend_and_platform() {
        let mut seen: Vec<(Backend, &str, &str)> = Vec::new();
        for a in ALL {
            for m in a.members {
                let key = (m.backend, a.os, a.arch);
                assert!(
                    !seen.contains(&key),
                    "duplicate manifest entry for {:?} on {}-{}",
                    m.backend,
                    a.os,
                    a.arch
                );
                seen.push(key);
            }
        }
    }

    /// The mechanism this task exists to add: on Windows x64, `ffmpeg` and
    /// `ffprobe` must resolve to the exact same `Asset` (same URL, same
    /// digest) — that identity is what lets `install::fetch_and_install`
    /// provision both from one download, regardless of which name `conv
    /// install` was given.
    #[test]
    fn ffmpeg_and_ffprobe_share_one_asset_on_windows_x64() {
        let ffmpeg = ALL
            .iter()
            .find(|a| {
                a.os == "windows"
                    && a.arch == "x64"
                    && a.members.iter().any(|m| m.backend == Backend::Ffmpeg)
            })
            .expect("windows-x64 ffmpeg entry must exist");
        let ffprobe = ALL
            .iter()
            .find(|a| {
                a.os == "windows"
                    && a.arch == "x64"
                    && a.members.iter().any(|m| m.backend == Backend::Ffprobe)
            })
            .expect("windows-x64 ffprobe entry must exist");
        assert!(
            std::ptr::eq(ffmpeg, ffprobe),
            "ffmpeg and ffprobe must resolve to the same manifest entry on windows-x64"
        );
        assert_eq!(ffmpeg.members.len(), 2);
    }

    /// The Linux/macOS side of the same coin: upstream ships ffmpeg and
    /// ffprobe there as two genuinely separate downloads (different URLs),
    /// so unlike Windows, each stays a one-member `Asset` and `bundled_with`
    /// must report nothing extra.
    #[test]
    fn ffmpeg_and_ffprobe_are_not_bundled_off_windows() {
        for (os, arch) in [("linux", "x64"), ("macos", "x64"), ("macos", "arm64")] {
            let asset = ALL
                .iter()
                .find(|a| {
                    a.os == os
                        && a.arch == arch
                        && a.members.iter().any(|m| m.backend == Backend::Ffmpeg)
                })
                .unwrap_or_else(|| panic!("{os}-{arch} ffmpeg entry must exist"));
            assert_eq!(
                asset.members.len(),
                1,
                "{os}-{arch}: ffmpeg must not be bundled with anything else"
            );
        }
    }

    #[test]
    fn bundled_with_is_empty_when_there_is_no_manifest_entry() {
        // Magick has zero manifest rows anywhere.
        assert!(bundled_with(Backend::Magick).is_empty());
    }

    #[test]
    fn bundled_with_is_empty_for_a_single_member_asset() {
        let covered = matches!(
            (current_os(), current_arch()),
            ("windows", "x64") | ("linux", "x64") | ("macos", "x64") | ("macos", "arm64")
        );
        if covered {
            assert!(bundled_with(Backend::Pandoc).is_empty());
        }
    }

    #[test]
    fn bundled_with_reports_the_sibling_on_windows_x64() {
        if (current_os(), current_arch()) == ("windows", "x64") {
            assert_eq!(bundled_with(Backend::Ffmpeg), vec![Backend::Ffprobe]);
            assert_eq!(bundled_with(Backend::Ffprobe), vec![Backend::Ffmpeg]);
        }
    }

    #[test]
    fn has_managed_build_is_false_for_magick_on_every_platform() {
        // Deterministic regardless of which platform this test runs on:
        // `magick` has zero manifest rows anywhere (see the module docs),
        // even though `Backend::is_managed()` is true for it.
        assert!(Backend::Magick.is_managed());
        assert!(!has_managed_build(Backend::Magick));
    }

    #[test]
    fn has_managed_build_is_false_for_soffice() {
        // is_managed() alone already rules this out, but has_managed_build
        // must agree.
        assert!(!Backend::Soffice.is_managed());
        assert!(!has_managed_build(Backend::Soffice));
    }

    #[test]
    fn has_managed_build_agrees_with_lookup_on_a_covered_platform() {
        let covered = matches!(
            (current_os(), current_arch()),
            ("windows", "x64") | ("linux", "x64") | ("macos", "x64") | ("macos", "arm64")
        );
        if covered {
            assert!(has_managed_build(Backend::Pandoc));
        }
    }

    /// Typst is verified for all four platforms this manifest covers —
    /// unlike ffmpeg/pandoc, which have no `linux`/`arm64` row.
    #[test]
    fn typst_has_a_manifest_entry_for_every_platform_this_manifest_covers() {
        for (os, arch) in [
            ("windows", "x64"),
            ("linux", "x64"),
            ("macos", "x64"),
            ("macos", "arm64"),
        ] {
            assert!(
                ALL.iter().any(|a| a.os == os
                    && a.arch == arch
                    && a.members.iter().any(|m| m.backend == Backend::Typst)),
                "missing a typst manifest entry for {os}-{arch}"
            );
        }
    }

    #[test]
    fn has_managed_build_is_true_for_typst_on_a_covered_platform() {
        let covered = matches!(
            (current_os(), current_arch()),
            ("windows", "x64") | ("linux", "x64") | ("macos", "x64") | ("macos", "arm64")
        );
        if covered {
            assert!(has_managed_build(Backend::Typst));
        }
    }

    #[test]
    fn lookup_finds_the_entry_for_the_running_platform_when_one_exists() {
        // ffmpeg and pandoc are verified for exactly these four (os, arch)
        // pairs — not, say, linux-arm64, a standard CI runner class that
        // this manifest simply doesn't cover yet. Review finding 4: the
        // original version of this test guarded on `current_os()` alone,
        // which on aarch64 Linux would wrongly assert an entry exists where
        // none does and fail.
        let covered = matches!(
            (current_os(), current_arch()),
            ("windows", "x64") | ("linux", "x64") | ("macos", "x64") | ("macos", "arm64")
        );
        if covered {
            let found = lookup(Backend::Pandoc);
            assert!(
                found.is_some(),
                "expected a verified pandoc entry for {}-{}",
                current_os(),
                current_arch()
            );
        }
    }
}
