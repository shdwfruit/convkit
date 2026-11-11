use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::*;

const WRITABLE: [ArchiveFormat; 6] = [
    ArchiveFormat::Zip,
    ArchiveFormat::Tar,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarBz2,
    ArchiveFormat::TarZst,
];

fn yes() -> impl FnMut(&Expansion) -> bool {
    |_| true
}

fn no_question() -> impl FnMut(&Expansion) -> bool {
    |e| panic!("asked about {e:?}")
}

/// A folder `photos` with a nested file, an empty folder, an executable,
/// a non-ASCII name, a link inside it, and a known time.
fn make_tree(root: &Path) -> PathBuf {
    let top = root.join("photos");
    fs::create_dir_all(top.join("sub")).unwrap();
    fs::create_dir_all(top.join("empty")).unwrap();
    fs::write(top.join("a.txt"), "hello").unwrap();
    fs::write(top.join("sub/b.txt"), "line\n".repeat(20_000)).unwrap();
    fs::write(top.join("café.txt"), "accent").unwrap();
    fs::write(top.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(top.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", top.join("latest")).unwrap();
    }
    let t = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(top.join("a.txt"), t).unwrap();
    top
}

fn check_tree(top: &Path) {
    assert_eq!(fs::read_to_string(top.join("a.txt")).unwrap(), "hello");
    assert_eq!(
        fs::read_to_string(top.join("sub/b.txt")).unwrap(),
        "line\n".repeat(20_000)
    );
    assert_eq!(fs::read_to_string(top.join("café.txt")).unwrap(), "accent");
    assert!(top.join("empty").is_dir());
    let mtime = fs::metadata(top.join("a.txt")).unwrap().modified().unwrap();
    assert_eq!(time::unix_secs(mtime), 1_600_000_000, "mtime kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(top.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bit kept: {mode:o}");
        let link = top.join("latest");
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("a.txt"));
    }
}

#[test]
fn a_multi_dot_name_is_told_by_its_longest_suffix() {
    let f = |s: &str| ArchiveFormat::from_path(Path::new(s));
    assert_eq!(f("photos.tar.gz"), Some(ArchiveFormat::TarGz));
    assert_eq!(f("backup.2026.TAR.GZ"), Some(ArchiveFormat::TarGz));
    assert_eq!(f("a.tgz"), Some(ArchiveFormat::TarGz));
    assert_eq!(f("a.tar.zst"), Some(ArchiveFormat::TarZst));
    assert_eq!(f("a.tar"), Some(ArchiveFormat::Tar));
    assert_eq!(f("a.7z"), Some(ArchiveFormat::SevenZ));
    // A plain gzip is one file, not an archive; a bare `.zip` is the
    // extension shorthand, not a file name.
    assert_eq!(f("notes.gz"), None);
    assert_eq!(f(".zip"), None);
    assert_eq!(f("photo.jpg"), None);
    assert_eq!(
        ArchiveFormat::stem(Path::new("dir/backup.2026.tar.gz")).as_deref(),
        Some("backup.2026")
    );
    assert_eq!(
        ArchiveFormat::from_name(".tar.xz"),
        Some(ArchiveFormat::TarXz)
    );
    assert_eq!(
        ArchiveFormat::from_name("TBZ2"),
        Some(ArchiveFormat::TarBz2)
    );
    assert_eq!(ArchiveFormat::from_name("gz"), None);
}

#[test]
fn every_writable_format_round_trips_a_folder() {
    for format in WRITABLE {
        let dir = tempfile::tempdir().unwrap();
        let top = make_tree(dir.path());
        let out = dir.path().join(format!("photos.{}", format.ext()));
        let made = pack(
            std::slice::from_ref(&top),
            &out,
            format,
            &PackOptions::default(),
        )
        .unwrap();
        assert_eq!(made.output, out);
        assert!(made.bytes > 0);
        assert_eq!(made.files, 4, "{format:?}");
        #[cfg(unix)]
        assert_eq!(made.links, 1, "{format:?}");

        fs::remove_dir_all(&top).unwrap();
        let opened = unpack(
            &out,
            &Destination::Beside(dir.path().to_path_buf()),
            &UnpackOptions::default(),
            &mut no_question(),
        )
        .unwrap();
        // One folder at the top: it unpacks as that folder, not
        // photos/photos.
        assert_eq!(opened.output, top, "{format:?}");
        check_tree(&top);
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().starts_with(".convkit"))
            .collect();
        assert!(left.is_empty(), "scratch left behind: {left:?}");
    }
}

#[test]
fn every_format_converts_to_every_other() {
    for from in WRITABLE {
        for to in WRITABLE.into_iter().filter(|t| *t != from) {
            let dir = tempfile::tempdir().unwrap();
            let top = make_tree(dir.path());
            let a = dir.path().join(format!("a.{}", from.ext()));
            pack(
                std::slice::from_ref(&top),
                &a,
                from,
                &PackOptions::default(),
            )
            .unwrap();
            fs::remove_dir_all(&top).unwrap();
            let b = dir.path().join(format!("b.{}", to.ext()));
            convert(
                &a,
                &b,
                to,
                &UnpackOptions::default(),
                &PackOptions::default(),
                &mut no_question(),
            )
            .unwrap();
            let out = dir.path().join("out");
            unpack(
                &b,
                &Destination::Into(out.clone()),
                &UnpackOptions::default(),
                &mut no_question(),
            )
            .unwrap();
            check_tree(&out.join("photos"));
        }
    }
}

#[test]
fn several_items_unpack_into_a_folder_named_after_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a").unwrap();
    fs::write(dir.path().join("b.txt"), "b").unwrap();
    let out = dir.path().join("pair.zip");
    pack(
        &[dir.path().join("a.txt"), dir.path().join("b.txt")],
        &out,
        ArchiveFormat::Zip,
        &PackOptions::default(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let r = unpack(
        &out,
        &Destination::Beside(dest.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(r.output, dest.path().join("pair"));
    assert_eq!(fs::read_to_string(r.output.join("b.txt")).unwrap(), "b");
}

#[test]
fn one_file_unpacks_as_that_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("report.pdf"), "%PDF").unwrap();
    let out = dir.path().join("report.zip");
    pack(
        &[dir.path().join("report.pdf")],
        &out,
        ArchiveFormat::Zip,
        &PackOptions::default(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let r = unpack(
        &out,
        &Destination::Beside(dest.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(r.output, dest.path().join("report.pdf"));
    assert!(r.output.is_file());
}

#[test]
fn an_existing_archive_is_not_overwritten_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let out = dir.path().join("photos.zip");
    fs::write(&out, "keep me").unwrap();
    let e = pack(
        std::slice::from_ref(&top),
        &out,
        ArchiveFormat::Zip,
        &PackOptions::default(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::OutputExists);
    assert_eq!(fs::read_to_string(&out).unwrap(), "keep me");
    let opts = PackOptions {
        overwrite: true,
        ..Default::default()
    };
    pack(&[top], &out, ArchiveFormat::Zip, &opts).unwrap();
    assert_ne!(fs::read(&out).unwrap(), b"keep me");
}

#[test]
fn unpacking_over_existing_files_is_refused_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let out = dir.path().join("photos.tar.gz");
    pack(
        std::slice::from_ref(&top),
        &out,
        ArchiveFormat::TarGz,
        &PackOptions::default(),
    )
    .unwrap();
    fs::write(top.join("a.txt"), "mine").unwrap();
    let beside = Destination::Beside(dir.path().to_path_buf());
    let e = unpack(&out, &beside, &UnpackOptions::default(), &mut no_question()).unwrap_err();
    assert_eq!(e.code, ErrorCode::OutputExists, "{}", e.message);
    assert!(e.message.contains("a.txt"), "{}", e.message);
    assert_eq!(fs::read_to_string(top.join("a.txt")).unwrap(), "mine");

    let opts = UnpackOptions {
        overwrite: true,
        ..Default::default()
    };
    let r = unpack(&out, &beside, &opts, &mut no_question()).unwrap();
    assert_eq!(fs::read_to_string(top.join("a.txt")).unwrap(), "hello");
    assert!(
        r.notes.iter().any(|n| n.contains("Replaced")),
        "{:?}",
        r.notes
    );
}

#[test]
fn unpacking_into_an_existing_folder_adds_to_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("new.txt"), "new").unwrap();
    let out = dir.path().join("one.zip");
    pack(
        &[dir.path().join("new.txt")],
        &out,
        ArchiveFormat::Zip,
        &PackOptions::default(),
    )
    .unwrap();
    let dest = dir.path().join("dest");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("old.txt"), "old").unwrap();
    unpack(
        &out,
        &Destination::Into(dest.clone()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(dest.join("old.txt")).unwrap(), "old");
    assert_eq!(fs::read_to_string(dest.join("new.txt")).unwrap(), "new");
}

#[test]
fn finder_files_are_left_out_of_a_new_archive_and_said() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("docs");
    fs::create_dir_all(top.join("__MACOSX")).unwrap();
    fs::write(top.join("__MACOSX/._x"), "fork").unwrap();
    fs::write(top.join(".DS_Store"), "view").unwrap();
    fs::write(top.join("x.txt"), "x").unwrap();
    fs::write(top.join("._x.txt"), "extra").unwrap();
    // `._` with no file it belongs to is someone's own file.
    fs::write(top.join("._alone"), "mine").unwrap();
    let out = dir.path().join("docs.zip");
    let r = pack(&[top], &out, ArchiveFormat::Zip, &PackOptions::default()).unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("Left out 3 files that only Finder uses")),
        "{:?}",
        r.notes
    );
    let listing = list(&out, None).unwrap();
    let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["docs/", "docs/._alone", "docs/x.txt"]);
}

/// Writes a zip whose entries are given as (name, data); a name the zip
/// crate's writer takes as it is.
fn raw_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

/// A tar of raw headers, so a name can be anything, `../` and `/` too.
/// (name, kind, data, link target)
type RawEntry<'a> = (&'a [u8], tar::EntryType, &'a [u8], &'a [u8]);

fn raw_tar(path: &Path, entries: &[RawEntry]) {
    let mut out = Vec::new();
    for (name, kind, data, link) in entries {
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link);
        h.set_entry_type(*kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    fs::write(path, out).unwrap();
}

/// Unpacks `archive` into a fresh `dest/inner` and checks it was refused
/// as unsafe, with nothing written anywhere under `dest`.
fn assert_refused(dir: &Path, archive: &Path) {
    let dest = dir.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let e = unpack(
        archive,
        &Destination::Into(dest.join("inner")),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsafeArchive, "{}", e.message);
    assert!(e.message.contains("nothing was unpacked"), "{}", e.message);
    let left: Vec<_> = fs::read_dir(&dest)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    assert!(!dir.join("evil.txt").exists());
}

#[test]
fn a_zip_slip_name_is_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("slip.zip");
    raw_zip(&z, &[("ok.txt", b"fine"), ("../../evil.txt", b"gotcha")]);
    assert_refused(dir.path(), &z);
}

#[test]
fn a_backslash_zip_slip_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("slip.zip");
    raw_zip(&z, &[("..\\..\\evil.txt", b"gotcha")]);
    assert_refused(dir.path(), &z);
}

#[test]
fn an_absolute_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("abs.tar");
    let evil = dir.path().join("evil.txt");
    let name = evil.to_string_lossy().into_owned();
    raw_tar(
        &t,
        &[(name.as_bytes(), tar::EntryType::Regular, b"gotcha", b"")],
    );
    assert_refused(dir.path(), &t);
}

#[test]
fn a_tar_slip_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("slip.tar");
    raw_tar(
        &t,
        &[
            (b"ok.txt", tar::EntryType::Regular, b"fine", b""),
            (
                b"a/../../../evil.txt",
                tar::EntryType::Regular,
                b"gotcha",
                b"",
            ),
        ],
    );
    assert_refused(dir.path(), &t);
}

#[cfg(unix)]
#[test]
fn a_link_out_of_the_folder_is_refused() {
    for target in [&b"../../.."[..], b"/etc", b"sub/../../.."] {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("link.tar");
        raw_tar(&t, &[(b"out", tar::EntryType::Symlink, b"", target)]);
        assert_refused(dir.path(), &t);
    }
}

/// The classic: a link to outside, then a file written "through" it.
#[cfg(unix)]
#[test]
fn a_file_written_through_an_escaping_link_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("through.tar");
    let outside = dir.path().to_string_lossy().into_owned();
    raw_tar(
        &t,
        &[
            (b"door", tar::EntryType::Symlink, b"", outside.as_bytes()),
            (b"door/evil.txt", tar::EntryType::Regular, b"gotcha", b""),
        ],
    );
    let dest = dir.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let e = unpack(
        &t,
        &Destination::Into(dest.join("inner")),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap_err();
    assert!(
        matches!(
            e.code,
            ErrorCode::UnsafeArchive | ErrorCode::ConversionFailed
        ),
        "{e:?}"
    );
    assert!(!dir.path().join("evil.txt").exists());
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
}

/// `b -> .` is harmless alone, and `a -> b/..` reads as `.`; followed for
/// real, `a` is the folder's parent.
#[cfg(unix)]
#[test]
fn a_link_that_escapes_only_through_another_link_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("chain.tar");
    raw_tar(
        &t,
        &[
            (b"a", tar::EntryType::Symlink, b"", b"b/.."),
            (b"b", tar::EntryType::Symlink, b"", b"."),
        ],
    );
    assert_refused(dir.path(), &t);
}

#[cfg(unix)]
#[test]
fn a_zip_link_out_of_the_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("link.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&z).unwrap());
    zip.add_symlink(
        "passwd",
        "/etc/passwd",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.finish().unwrap();
    assert_refused(dir.path(), &z);
}

#[test]
fn a_hard_link_out_of_the_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("hard.tar");
    raw_tar(
        &t,
        &[(b"h", tar::EntryType::Link, b"", b"../../etc/passwd")],
    );
    assert_refused(dir.path(), &t);
}

#[cfg(unix)]
#[test]
fn links_that_stay_inside_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("ok.tar");
    raw_tar(
        &t,
        &[
            (b"top/sub/f.txt", tar::EntryType::Regular, b"f", b""),
            (b"top/up", tar::EntryType::Symlink, b"", b"sub/../sub/f.txt"),
            (b"top/sub/back", tar::EntryType::Symlink, b"", b"../up"),
            (b"top/hard", tar::EntryType::Link, b"", b"top/sub/f.txt"),
        ],
    );
    let r = unpack(
        &t,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(r.links, 2);
    assert_eq!(fs::read_to_string(r.output.join("sub/back")).unwrap(), "f");
    assert_eq!(fs::read_to_string(r.output.join("hard")).unwrap(), "f");
}

fn zeros_zip(path: &Path, bytes: usize) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("zeros.bin", opts).unwrap();
    zip.write_all(&vec![0u8; bytes]).unwrap();
    zip.finish().unwrap();
}

#[test]
fn a_zip_bomb_is_asked_about_before_anything_is_written() {
    safe::TEST_LIMITS.with(|l| l.set(Some((100_000, 100))));
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("bomb.zip");
    zeros_zip(&z, 4_000_000);
    let mut asked = None;
    let e = unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut |x: &Expansion| {
            asked = Some(x.clone());
            false
        },
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    let asked = asked.expect("asked first");
    assert!(asked.listed, "zip lists its size up front");
    assert_eq!(asked.unpacked, 4_000_000);
    assert!(asked.ratio() > 100, "{asked:?}");
    assert!(asked.warning().contains("zip bomb"), "{}", asked.warning());
    let names: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "only the zip itself is left");

    // A yes unpacks it.
    let r = unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut yes(),
    )
    .unwrap();
    assert_eq!(fs::metadata(&r.output).unwrap().len(), 4_000_000);
    safe::TEST_LIMITS.with(|l| l.set(None));
}

/// A compressed tar lists nothing up front, so the question comes as the
/// unpacked size passes the line.
#[test]
fn a_tar_bomb_is_asked_about_as_it_grows() {
    safe::TEST_LIMITS.with(|l| l.set(Some((100_000, 100))));
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("zeros.bin"), vec![0u8; 4_000_000]).unwrap();
    let t = dir.path().join("bomb.tar.gz");
    pack(
        &[dir.path().join("zeros.bin")],
        &t,
        ArchiveFormat::TarGz,
        &PackOptions::default(),
    )
    .unwrap();
    fs::remove_file(dir.path().join("zeros.bin")).unwrap();
    let mut asked = None;
    let e = unpack(
        &t,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut |x: &Expansion| {
            asked = Some(x.clone());
            false
        },
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert!(!asked.unwrap().listed);
    assert!(!dir.path().join("zeros.bin").exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    safe::TEST_LIMITS.with(|l| l.set(None));
}

#[test]
fn an_ordinary_archive_is_not_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("small.zip");
    zeros_zip(&z, 4_000_000);
    unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
}

#[test]
fn a_password_zip_needs_its_password() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let z = dir.path().join("locked.zip");
    let opts = PackOptions {
        password: Some("correct horse".into()),
        ..Default::default()
    };
    let made = pack(std::slice::from_ref(&top), &z, ArchiveFormat::Zip, &opts).unwrap();
    assert!(made.encrypted);
    assert!(made.notes.iter().any(|n| n.contains("AES-256")));
    fs::remove_dir_all(&top).unwrap();
    let beside = Destination::Beside(dir.path().to_path_buf());

    let e = unpack(&z, &beside, &UnpackOptions::default(), &mut no_question()).unwrap_err();
    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert!(e.message.contains("password-protected"), "{}", e.message);

    let wrong = UnpackOptions {
        password: Some("wrong".into()),
        ..Default::default()
    };
    let e = unpack(&z, &beside, &wrong, &mut no_question()).unwrap_err();
    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert!(e.message.contains("wrong password"), "{}", e.message);
    assert!(!top.exists());

    let right = UnpackOptions {
        password: Some("correct horse".into()),
        ..Default::default()
    };
    let r = unpack(&z, &beside, &right, &mut no_question()).unwrap();
    assert!(r.encrypted);
    check_tree(&top);
}

#[test]
fn a_password_on_a_tar_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let opts = PackOptions {
        password: Some("x".into()),
        ..Default::default()
    };
    let e = pack(
        &[top],
        &dir.path().join("a.tar.gz"),
        ArchiveFormat::TarGz,
        &opts,
    )
    .unwrap_err();
    assert!(e.message.contains("zip archives only"), "{}", e.message);
}

#[test]
fn converting_a_locked_zip_says_the_result_is_not_locked() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let z = dir.path().join("locked.zip");
    let lock = PackOptions {
        password: Some("pw".into()),
        ..Default::default()
    };
    pack(&[top], &z, ArchiveFormat::Zip, &lock).unwrap();
    let read = UnpackOptions {
        password: Some("pw".into()),
        ..Default::default()
    };
    let r = convert(
        &z,
        &dir.path().join("open.tar.gz"),
        ArchiveFormat::TarGz,
        &read,
        &PackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("the new archive isn't")),
        "{:?}",
        r.notes
    );
}

#[test]
fn macosx_is_left_out_when_unpacking() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("finder.zip");
    raw_zip(
        &z,
        &[("photos/a.txt", b"a"), ("__MACOSX/photos/._a.txt", b"fork")],
    );
    let r = unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(r.output, dir.path().join("photos"));
    assert!(!dir.path().join("__MACOSX").exists());
    assert!(r.notes.iter().any(|n| n.contains("__MACOSX")));
}

/// An old Windows zip: the name is CP437 bytes, with no UTF-8 flag.
#[test]
fn a_name_that_is_not_utf8_is_read_as_dos_text() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("old.zip");
    raw_zip(&z, &[("cafX.txt", b"old")]);
    let mut bytes = fs::read(&z).unwrap();
    let mut patched = 0;
    for i in 0..bytes.len() - 8 {
        if &bytes[i..i + 8] == b"cafX.txt" {
            bytes[i + 3] = 0x82; // CP437 for é
            patched += 1;
        }
    }
    assert_eq!(patched, 2, "local and central names");
    fs::write(&z, bytes).unwrap();
    let r = unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(r.output, dir.path().join("café.txt"));
    assert!(r.notes.iter().any(|n| n.contains("CP437")), "{:?}", r.notes);
}

#[test]
fn a_7z_unpacks_and_converts() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    // 7-Zip's own writer does not keep links; drop the one in the tree.
    let _ = fs::remove_file(top.join("latest"));
    let s = dir.path().join("photos.7z");
    sevenz_rust2::compress_to_path(&top, &s).unwrap();
    let out = dir.path().join("out");
    let r = unpack(
        &s,
        &Destination::Into(out.clone()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert!(r.files >= 4, "{r:?}");
    assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "hello");
    assert_eq!(
        fs::read_to_string(out.join("sub/b.txt")).unwrap(),
        "line\n".repeat(20_000)
    );
    convert(
        &s,
        &dir.path().join("photos.zip"),
        ArchiveFormat::Zip,
        &UnpackOptions::default(),
        &PackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
}

#[test]
fn a_locked_7z_needs_its_password() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let _ = fs::remove_file(top.join("latest"));
    let s = dir.path().join("locked.7z");
    sevenz_rust2::compress_to_path_encrypted(&top, &s, sevenz_rust2::Password::new("pw")).unwrap();
    let into = Destination::Into(dir.path().join("out"));
    let e = unpack(&s, &into, &UnpackOptions::default(), &mut no_question()).unwrap_err();
    assert_eq!(e.code, ErrorCode::PasswordProtected, "{e:?}");
    let right = UnpackOptions {
        password: Some("pw".into()),
        ..Default::default()
    };
    let r = unpack(&s, &into, &right, &mut no_question()).unwrap();
    assert!(r.encrypted);
}

#[test]
fn rar_and_writing_7z_are_refused_with_what_to_use() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path().join("x.rar");
    fs::write(&r, "Rar!").unwrap();
    let e = unpack(
        &r,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedPair);
    assert!(
        e.remediation
            .unwrap()
            .manual
            .unwrap()
            .contains("The Unarchiver"),
        "names a tool that opens it"
    );
    let top = make_tree(dir.path());
    let e = pack(
        &[top],
        &dir.path().join("x.7z"),
        ArchiveFormat::SevenZ,
        &PackOptions::default(),
    )
    .unwrap_err();
    assert!(e.message.contains("doesn't make them"), "{}", e.message);
}

#[test]
fn packing_a_folder_into_itself_leaves_the_archive_out() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let out = top.join("photos.zip");
    pack(
        std::slice::from_ref(&top),
        &out,
        ArchiveFormat::Zip,
        &PackOptions::default(),
    )
    .unwrap();
    let listing = list(&out, None).unwrap();
    assert!(
        !listing
            .entries
            .iter()
            .any(|e| e.name.contains(".zip") || e.name.contains(".convkit")),
        "{:?}",
        listing.entries
    );
}

#[cfg(unix)]
#[test]
fn a_link_out_of_what_is_packed_is_kept_and_said() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    std::os::unix::fs::symlink("/etc/hosts", top.join("hosts")).unwrap();
    let r = pack(
        &[top],
        &dir.path().join("p.tar"),
        ArchiveFormat::Tar,
        &PackOptions::default(),
    )
    .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("photos/hosts")),
        "{:?}",
        r.notes
    );
}

#[test]
fn the_listing_names_the_top_level() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    let out = dir.path().join("p.tar.xz");
    pack(&[top], &out, ArchiveFormat::TarXz, &PackOptions::default()).unwrap();
    let l = list(&out, None).unwrap();
    assert_eq!(l.top_level(), ["photos"]);
    assert!(l.total() > 100_000);
}

fn refuses() -> impl FnMut(&Expansion) -> bool {
    |_| false
}

#[test]
fn a_hard_link_copied_on_a_disk_without_links_counts_toward_the_question() {
    safe::TEST_LIMITS.with(|l| l.set(Some((10_000, 2))));
    safe::TEST_NO_HARDLINKS.with(|l| l.set(true));
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("links.tar");
    let names: Vec<String> = (0..20).map(|i| format!("l{i}")).collect();
    let mut entries: Vec<RawEntry> = vec![(b"f", tar::EntryType::Regular, &[7u8; 20_000], b"")];
    for n in &names {
        entries.push((n.as_bytes(), tar::EntryType::Link, b"", b"f"));
    }
    raw_tar(&t, &entries);
    let e = unpack(
        &t,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut refuses(),
    )
    .unwrap_err();
    safe::TEST_LIMITS.with(|l| l.set(None));
    safe::TEST_NO_HARDLINKS.with(|l| l.set(false));
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
}

#[test]
fn many_empty_files_are_asked_about() {
    safe::TEST_LIMITS.with(|l| l.set(Some((100_000, 10))));
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("empties");
    fs::create_dir(&src).unwrap();
    for i in 0..3000 {
        fs::write(src.join(format!("f{i}")), "").unwrap();
    }
    let t = dir.path().join("empties.tar.gz");
    pack(&[src], &t, ArchiveFormat::TarGz, &PackOptions::default()).unwrap();
    let e = unpack(
        &t,
        &Destination::Into(dir.path().join("out")),
        &UnpackOptions::default(),
        &mut refuses(),
    )
    .unwrap_err();
    safe::TEST_LIMITS.with(|l| l.set(None));
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
}

#[test]
fn names_that_differ_only_in_case_are_refused_on_a_disk_that_ignores_case() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("probe"), "").unwrap();
    if !dir.path().join("PROBE").exists() {
        return;
    }
    let z = dir.path().join("c.zip");
    raw_zip(&z, &[("top/Makefile", b"one"), ("top/makefile", b"two")]);
    let e = unpack(
        &z,
        &Destination::Into(dir.path().join("out")),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap_err();
    assert!(
        e.message.contains("Makefile") && e.message.contains("makefile"),
        "{}",
        e.message
    );
}

#[test]
fn a_repeated_name_still_keeps_the_later_entry() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("d.tar");
    raw_tar(
        &z,
        &[
            (b"top/a", tar::EntryType::Regular, b"one", b""),
            (b"top/a", tar::EntryType::Regular, b"two", b""),
        ],
    );
    let r = unpack(
        &z,
        &Destination::Into(dir.path().join("out")),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert_eq!(fs::read(r.output.join("top/a")).unwrap(), b"two");
}

#[test]
fn a_macos_sidecar_does_not_hide_the_one_top_folder() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("photos.zip");
    raw_zip(&z, &[("photos/a.txt", b"hi"), ("._photos", b"x")]);
    let r = unpack(
        &z,
        &Destination::Beside(dir.path().to_path_buf()),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert!(r.output.join("a.txt").exists(), "{:?}", r.output);
}

#[cfg(unix)]
#[test]
fn unpacking_into_a_link_to_a_folder_goes_into_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let z = dir.path().join("a.zip");
    raw_zip(&z, &[("a.txt", b"hi")]);
    unpack(
        &z,
        &Destination::Into(link),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap();
    assert!(real.join("a.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_failed_merge_moves_back_what_it_moved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest");
    fs::create_dir_all(dest.join("zz")).unwrap();
    fs::set_permissions(dest.join("zz"), fs::Permissions::from_mode(0o500)).unwrap();
    let writable = fs::write(dest.join("zz/probe"), "").is_ok();
    let z = dir.path().join("a.zip");
    raw_zip(&z, &[("a.txt", b"1"), ("b.txt", b"2"), ("zz/c.txt", b"3")]);
    let r = unpack(
        &z,
        &Destination::Into(dest.clone()),
        &UnpackOptions::default(),
        &mut no_question(),
    );
    fs::set_permissions(dest.join("zz"), fs::Permissions::from_mode(0o700)).unwrap();
    if writable {
        return; // running as root: the folder was never read-only
    }
    assert!(r.is_err());
    assert!(!dest.join("a.txt").exists() && !dest.join("b.txt").exists());
}

#[test]
fn a_refused_unpack_leaves_no_new_folders() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("slip.zip");
    raw_zip(&z, &[("../evil.txt", b"x")]);
    unpack(
        &z,
        &Destination::Into(dir.path().join("new/deep/dest")),
        &UnpackOptions::default(),
        &mut no_question(),
    )
    .unwrap_err();
    assert!(!dir.path().join("new").exists());
}

#[test]
fn listing_a_sparse_tar_does_not_read_its_claimed_size() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_path("big").unwrap();
    h.set_entry_type(tar::EntryType::GNUSparse);
    h.set_size(512);
    let gnu = h.as_gnu_mut().unwrap();
    gnu.set_real_size(1 << 40);
    // One 512-byte chunk at the very end; everything before it is a hole.
    gnu.sparse[0].offset[0] = 0x80;
    gnu.sparse[0].offset[4..].copy_from_slice(&((1u64 << 40) - 512).to_be_bytes());
    gnu.sparse[0].numbytes.copy_from_slice(b"00000001000\0");
    h.set_cksum();
    let mut bytes = h.as_bytes().to_vec();
    bytes.resize(bytes.len() + 512 + 1024, 0);
    let t = dir.path().join("sparse.tar");
    fs::write(&t, bytes).unwrap();
    let start = std::time::Instant::now();
    let l = list(&t, None).unwrap();
    assert_eq!(l.entries.len(), 1);
    assert!(start.elapsed().as_secs() < 5);
}

#[test]
fn a_listing_total_saturates() {
    let e = |n: &str| Listed {
        name: n.into(),
        kind: "file",
        size: u64::MAX,
    };
    let l = Listing {
        format: ArchiveFormat::Zip,
        entries: vec![e("a"), e("b")],
        encrypted: false,
    };
    assert_eq!(l.total(), u64::MAX);
}

#[cfg(unix)]
#[test]
fn a_link_to_a_top_level_name_that_is_not_packed_is_said() {
    let dir = tempfile::tempdir().unwrap();
    let top = make_tree(dir.path());
    std::os::unix::fs::symlink("../notes.txt", top.join("notes")).unwrap();
    let r = pack(
        &[top],
        &dir.path().join("p.tar"),
        ArchiveFormat::Tar,
        &PackOptions::default(),
    )
    .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("photos/notes")),
        "{:?}",
        r.notes
    );
}
